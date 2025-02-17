use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use parity_wasm::elements::External;
use specs::brtable::ElemEntry;
use specs::brtable::ElemTable;
use specs::configure_table::ConfigureTable;
use specs::host_function::HostFunctionDesc;
use specs::host_function::HostPlugin;
use specs::imtable::InitMemoryTable;
use specs::imtable::InitMemoryTableEntry;
use specs::itable::InstructionTable;
use specs::itable::InstructionTableInternal;
use specs::mtable::LocationType;
use specs::mtable::VarType;
use specs::state::InitializationState;
use specs::step::StepInfo;
use specs::types::FunctionType;
use specs::types::ValueType;
use specs::CompilationTable;
use specs::ExecutionTable;
use specs::Tables;
use specs::TraceBackend;
use wasmi::func::FuncInstanceInternal;

use wasmi::isa::Instruction;
use wasmi::isa::Keep;
use wasmi::memory_units::Pages;
use wasmi::module::eval_init_expr;
use wasmi::monitor::Monitor;
use wasmi::runner::from_value_internal_to_u64_with_typ;
use wasmi::runner::FunctionContext;
use wasmi::runner::InstructionOutcome;
use wasmi::runner::ValueStack;
use wasmi::Error;
use wasmi::FuncRef;
use wasmi::RuntimeValue;
use wasmi::Signature;
use wasmi::Trap;
use wasmi::DEFAULT_VALUE_STACK_LIMIT;

use crate::circuits::compute_slice_capability;

use self::etable::ETable;
use self::frame_table::FrameTable;
use self::instruction::run_instruction_pre;
use self::instruction::FuncDesc;
use self::instruction::InstructionIntoOpcode;
use self::instruction::PhantomFunction;
use self::instruction::RunInstructionTracePre;

use super::phantom::PhantomHelper;

mod etable;
mod frame_table;
mod instruction;

const DEFAULT_MEMORY_INDEX: u32 = 0;
const DEFAULT_TABLE_INDEX: u32 = 0;

pub struct TablePlugin {
    capacity: u32,

    phantom_helper: PhantomHelper,

    host_function_desc: HashMap<usize, HostFunctionDesc>,
    pub(super) function_table: Vec<FuncDesc>,

    itable: InstructionTableInternal,
    elements: Vec<ElemEntry>,
    configure_table: ConfigureTable,
    init_memory_table: Vec<InitMemoryTableEntry>,
    start_fid: Option<u32>,

    etable: ETable,
    frame_table: FrameTable,
    last_jump_eid: Vec<u32>,

    module_ref: Option<wasmi::ModuleRef>,
    unresolved_event: Option<RunInstructionTracePre>,
}

impl TablePlugin {
    pub fn new(
        k: u32,
        host_function_desc: HashMap<usize, HostFunctionDesc>,
        phantom_regex: &[String],
        wasm_input: FuncRef,
        trace_backend: TraceBackend,
    ) -> Self {
        let capacity = compute_slice_capability(k);
        let trace_backend = Rc::new(trace_backend);

        Self {
            capacity,

            host_function_desc,

            phantom_helper: PhantomHelper::new(phantom_regex, wasm_input),

            itable: InstructionTableInternal::default(),
            elements: vec![],
            configure_table: ConfigureTable::default(),
            init_memory_table: vec![],
            function_table: vec![],
            start_fid: None,

            last_jump_eid: vec![],
            etable: ETable::new(capacity, trace_backend.clone()),
            frame_table: FrameTable::new(trace_backend),

            module_ref: None,
            unresolved_event: None,
        }
    }

    pub fn into_compilation_table(&self) -> CompilationTable {
        let itable: InstructionTable = self.itable.clone().into();
        let imtable = InitMemoryTable::new(self.init_memory_table.clone());
        let br_table = Arc::new(itable.create_brtable());
        let elem_table = Arc::new(ElemTable::new(self.elements.clone()));
        let configure_table = Arc::new(self.configure_table);
        let initialization_state = Arc::new(InitializationState {
            eid: 1,
            fid: self.start_fid.unwrap(),
            iid: 0,
            frame_id: 0,
            sp: DEFAULT_VALUE_STACK_LIMIT as u32 - 1,

            host_public_inputs: 1,
            context_in_index: 1,
            context_out_index: 1,
            external_host_call_call_index: 1,

            initial_memory_pages: configure_table.init_memory_pages,
            maximal_memory_pages: configure_table.maximal_memory_pages,
        });

        CompilationTable {
            itable: Arc::new(itable),
            imtable: Arc::new(imtable),
            br_table,
            elem_table,
            configure_table,
            initial_frame_table: Arc::new(self.frame_table.build_initial_frame_table()),
            initialization_state,
        }
    }

    pub fn into_tables(self) -> Tables {
        Tables {
            compilation_tables: self.into_compilation_table(),
            execution_tables: ExecutionTable {
                etable: self.etable.finalized(),
                frame_table: self.frame_table.finalized(),
            },
        }
    }
}

impl TablePlugin {
    fn push_event(
        &mut self,
        fid: u32,
        iid: u32,
        sp: u32,
        allocated_memory_pages: u32,
        last_jump_eid: u32,
        step_info: StepInfo,
    ) {
        if self.etable.entries().len() == self.capacity as usize {
            self.etable.flush();
            self.frame_table.flush();
        }

        self.etable.push(
            fid,
            iid,
            sp,
            allocated_memory_pages,
            last_jump_eid,
            step_info,
        )
    }

    fn push_frame(
        &mut self,
        frame_id: u32,
        next_frame_id: u32,
        callee_fid: u32,
        fid: u32,
        iid: u32,
    ) {
        self.frame_table
            .push(frame_id, next_frame_id, callee_fid, fid, iid);

        self.last_jump_eid.push(frame_id);
    }

    fn push_static_frame(
        &mut self,
        frame_id: u32,
        next_frame_id: u32,
        callee_fid: u32,
        fid: u32,
        iid: u32,
    ) {
        self.frame_table
            .push_static_entry(frame_id, next_frame_id, callee_fid, fid, iid);
    }

    fn pop_frame(&mut self) {
        self.frame_table.pop();
        self.last_jump_eid.pop();
    }

    pub fn fill_trace(
        &mut self,
        current_sp: u32,
        allocated_memory_pages: u32,
        fid: u32,
        callee_sig: &Signature,
        keep_value: Option<u64>,
        wasm_input_func_ref: FuncRef,
    ) {
        let has_return_value = callee_sig.return_type().is_some();

        let last_jump_eid = *self.last_jump_eid.last().unwrap();

        let mut iid = 0;

        let wasm_input_host_func_index = match wasm_input_func_ref.as_internal() {
            FuncInstanceInternal::Internal { .. } => unreachable!(),
            FuncInstanceInternal::Host {
                host_func_index, ..
            } => host_func_index,
        };

        if has_return_value {
            self.push_event(
                fid,
                iid,
                current_sp,
                allocated_memory_pages,
                last_jump_eid,
                StepInfo::I32Const { value: 0 },
            );

            iid += 1;

            self.push_event(
                fid,
                iid,
                current_sp + 1,
                allocated_memory_pages,
                last_jump_eid,
                StepInfo::CallHost {
                    plugin: HostPlugin::HostInput,
                    host_function_idx: *wasm_input_host_func_index,
                    function_name: "wasm_input".to_owned(),
                    signature: specs::host_function::Signature {
                        params: vec![ValueType::I32],
                        return_type: Some(ValueType::I64),
                    },
                    args: vec![0],
                    ret_val: Some(keep_value.unwrap()),
                    op_index_in_plugin: 0,
                },
            );

            iid += 1;

            if callee_sig.return_type() != Some(wasmi::ValueType::I64) {
                self.push_event(
                    fid,
                    iid,
                    current_sp + 1,
                    allocated_memory_pages,
                    last_jump_eid,
                    StepInfo::I32WrapI64 {
                        value: keep_value.unwrap() as i64,
                        result: keep_value.unwrap() as i32,
                    },
                );

                iid += 1;
            }
        }

        self.push_event(
            fid,
            iid,
            current_sp + has_return_value as u32,
            allocated_memory_pages,
            last_jump_eid,
            StepInfo::Return {
                drop: callee_sig.params().len() as u32,
                keep: if let Some(t) = callee_sig.return_type() {
                    vec![t.into_elements().into()]
                } else {
                    vec![]
                },
                keep_values: keep_value.map_or(vec![], |v| vec![v]),
            },
        );
    }
}

impl Monitor for TablePlugin {
    fn register_module(
        &mut self,
        module: &parity_wasm::elements::Module,
        module_ref: &wasmi::ModuleRef,
        entry: &str,
    ) -> Result<(), wasmi::Error> {
        self.module_ref = Some(module_ref.clone());

        self.phantom_helper
            .register_module(module, module_ref, entry)?;

        let mut type_idx_of_func = vec![];

        // register static frame entries
        {
            let zkmain = module_ref.func_by_name(entry)?;
            let zkmain_idx = match zkmain.as_internal() {
                FuncInstanceInternal::Internal { index, .. } => index,
                _ => unreachable!(),
            };

            self.push_static_frame(0, 0, *zkmain_idx as u32, 0, 0);

            if let Some(start_idx) = module.start_section() {
                self.push_static_frame(0, 0, start_idx, *zkmain_idx as u32, 0);

                self.start_fid = Some(start_idx);
            } else {
                self.start_fid = Some(*zkmain_idx as u32);
            }
        }

        {
            for import_entry in module.import_section().map(|s| s.entries()).unwrap_or(&[]) {
                if let External::Function(fn_ty_idx) = *import_entry.external() {
                    type_idx_of_func.push(fn_ty_idx);
                }
            }

            let funcs = module
                .function_section()
                .map(|fs| fs.entries())
                .unwrap_or(&[]);

            for func in funcs {
                type_idx_of_func.push(func.type_ref())
            }
        }

        // register functions
        {
            let mut fid = 0;

            while let Some(ref func) = module_ref.func_by_index(fid) {
                let ftype = match *func.as_internal() {
                    FuncInstanceInternal::Internal { .. } => FunctionType::WasmFunction,
                    FuncInstanceInternal::Host {
                        host_func_index, ..
                    } => match self
                        .host_function_desc
                        .get(&host_func_index)
                        .cloned()
                        .unwrap()
                    {
                        HostFunctionDesc::Internal {
                            name,
                            op_index_in_plugin,
                            plugin,
                        } => FunctionType::HostFunction {
                            plugin,
                            function_index: host_func_index,
                            function_name: name,
                            op_index_in_plugin,
                        },
                        HostFunctionDesc::External { name, op, sig } => {
                            FunctionType::HostFunctionExternal {
                                function_name: name,
                                op,
                                sig,
                            }
                        }
                    },
                };

                self.function_table.push(FuncDesc {
                    ftype,
                    signature: func.signature().clone(),
                });

                fid += 1;
            }

            let mut fid = 0;
            while let Some(ref func) = module_ref.func_by_index(fid) {
                let function_mapping = |index| self.function_table.get(index as usize).unwrap();

                if self.phantom_helper.is_phantom_function(fid) {
                    let wasm_input_func_idx = self.phantom_helper.wasm_input_func_idx(module_ref);

                    let instructions = PhantomFunction::build_phantom_function_instructions(
                        func.signature(),
                        wasm_input_func_idx,
                    );

                    for (iid, instruction) in instructions.into_iter().enumerate() {
                        self.itable.push(
                            fid,
                            iid as u32,
                            instruction.into_opcode(&function_mapping),
                        );
                    }
                } else if let Some(body) = func.body() {
                    let code = &body.code;

                    let mut iter = code.iterate_from(0);
                    let mut iid = iter.position();
                    while let Some(instr) = iter.next() {
                        self.itable
                            .push(fid, iid, instr.into_opcode(&function_mapping));

                        iid = iter.position();
                    }
                }

                fid += 1;
            }
        }

        // register globals
        for (global_idx, global_ref) in module_ref.globals().iter().enumerate() {
            self.init_memory_table.push(InitMemoryTableEntry {
                is_mutable: global_ref.is_mutable(),
                ltype: LocationType::Global,
                offset: global_idx as u32,
                vtype: global_ref.value_type().into_elements().into(),
                value: global_ref.get().into(),
                eid: 0,
            });
        }

        // register memory
        if let Some(memory_ref) = module_ref.memory_by_index(DEFAULT_MEMORY_INDEX) {
            // one page contains 64KB*1024/8=8192 u64 entries
            const ENTRIES: u32 = 8192;

            let init_memory_pages = memory_ref.initial().0 as u32;
            let maximal_memory_pages = memory_ref.maximum().map_or(65536, |max| max.0 as u32);

            self.configure_table = ConfigureTable {
                init_memory_pages,
                maximal_memory_pages,
            };

            for offset in 0..(init_memory_pages * ENTRIES) {
                let mut buf = [0u8; 8];
                memory_ref.get_into(offset * 8, &mut buf)?;

                let value = u64::from_le_bytes(buf);
                if value != 0 {
                    self.init_memory_table.push(InitMemoryTableEntry {
                        is_mutable: true,
                        ltype: LocationType::Heap,
                        offset,
                        vtype: VarType::I64,
                        value,
                        eid: 0,
                    });
                }
            }
        }

        // for data_segment in module.data_section().map(|ds| ds.entries()).unwrap_or(&[]) {
        //     let offset = data_segment
        //         .offset()
        //         .as_ref()
        //         .expect("passive segments are rejected due to validation");
        //     let offset_val = match eval_init_expr(offset, &module_ref) {
        //         RuntimeValue::I32(v) => v as u32,
        //         _ => panic!("Due to validation data segment offset should evaluate to i32"),
        //     };

        //     let memory_inst = module_ref
        //         .memory_by_index(DEFAULT_MEMORY_INDEX)
        //         .expect("Due to validation default memory should exists");
        //     memory_inst.set(offset_val, data_segment.value())?;

        //     todo!()
        // }

        for element_segment in module
            .elements_section()
            .map(|es| es.entries())
            .unwrap_or(&[])
        {
            let offset = element_segment
                .offset()
                .as_ref()
                .expect("passive segments are rejected due to validation");
            let offset_val = match eval_init_expr(offset, module_ref) {
                RuntimeValue::I32(v) => v as u32,
                _ => panic!("Due to validation elem segment offset should evaluate to i32"),
            };

            let table_inst = module_ref
                .table_by_index(DEFAULT_TABLE_INDEX)
                .expect("Due to validation default table should exists");

            // This check is not only for bailing out early, but also to check the case when
            // segment consist of 0 members.
            if offset_val as u64 + element_segment.members().len() as u64
                > table_inst.current_size() as u64
            {
                return Err(Error::Instantiation(
                    "elements segment does not fit".to_string(),
                ));
            }

            for (j, func_idx) in element_segment.members().iter().enumerate() {
                self.elements.push(ElemEntry {
                    table_idx: DEFAULT_TABLE_INDEX,
                    type_idx: type_idx_of_func[*func_idx as usize],
                    offset: offset_val + j as u32,
                    func_idx: *func_idx,
                });
            }
        }

        assert_eq!(self.function_table.len(), type_idx_of_func.len());

        Ok(())
    }

    fn invoke_exported_function_pre_hook(&mut self) {
        self.last_jump_eid.push(0);
    }

    fn invoke_instruction_pre_hook(
        &mut self,
        value_stack: &ValueStack,
        function_context: &FunctionContext,
        instruction: &Instruction,
    ) {
        // phantom should do nothing
        if self.phantom_helper.is_in_phantom_function() {
            return;
        }
        self.unresolved_event = run_instruction_pre(value_stack, function_context, instruction);
    }

    fn invoke_instruction_post_hook(
        &mut self,
        fid: u32,
        iid: u32,
        sp: u32,
        allocated_memory_pages: u32,
        value_stack: &ValueStack,
        function_context: &FunctionContext,
        instruction: &Instruction,
        outcome: &InstructionOutcome,
    ) -> Result<(), Trap> {
        if !self.phantom_helper.is_in_phantom_function() {
            let current_event = self.unresolved_event.take();

            let event = self.run_instruction_post(
                self.module_ref.as_ref().unwrap(),
                current_event,
                value_stack,
                function_context,
                instruction,
            );

            self.push_event(
                fid,
                iid,
                sp,
                allocated_memory_pages,
                *self.last_jump_eid.last().unwrap(),
                event,
            );
        }

        match outcome {
            InstructionOutcome::ExecuteCall(func_ref) => {
                if let FuncInstanceInternal::Internal { index, .. } = func_ref.as_internal() {
                    if !self.phantom_helper.is_in_phantom_function() {
                        let eid = self.etable.entries().last().unwrap().eid;

                        self.push_frame(
                            eid,
                            *self.last_jump_eid.last().unwrap(),
                            *index as u32,
                            fid,
                            iid + 1,
                        );
                    }

                    if self.phantom_helper.is_phantom_function(*index as u32) {
                        if let Some(memory_ref) = function_context.memory() {
                            let mut buf = memory_ref.buffer_cache.borrow_mut();

                            if buf.is_none() {
                                *buf = Some((
                                    Pages(allocated_memory_pages as usize),
                                    HashMap::default(),
                                ));
                            }
                        }
                        self.phantom_helper.push_frame(value_stack.len() as u32);
                    }
                }
            }
            InstructionOutcome::Return(dropkeep) => {
                if self.phantom_helper.is_phantom_function(fid) {
                    let sp_before = self.phantom_helper.pop_frame().unwrap();

                    let wasm_input = self.phantom_helper.wasm_input.clone();
                    let signature = self.function_table[fid as usize].signature.clone();
                    if !self.phantom_helper.is_in_phantom_function() {
                        let allocated_memory_pages =
                            if let Some(memory_ref) = function_context.memory() {
                                let mut buf = memory_ref.buffer_cache.borrow_mut();

                                let pages = buf.as_ref().unwrap().0;
                                *buf = None;
                                memory_ref.shrink(pages).unwrap();

                                pages.0
                            } else {
                                0
                            };

                        self.fill_trace(
                            sp_before,
                            allocated_memory_pages as u32,
                            fid,
                            &signature,
                            if let Keep::Single(t) = dropkeep.keep {
                                Some(from_value_internal_to_u64_with_typ(
                                    t.into(),
                                    *value_stack.top(),
                                ))
                            } else {
                                None
                            },
                            wasm_input,
                        )
                    }
                }

                if !self.phantom_helper.is_in_phantom_function() {
                    self.pop_frame();
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn invoke_call_host_post_hook(&mut self, return_value: Option<RuntimeValue>) {
        if self.phantom_helper.is_in_phantom_function() {
            return;
        }

        if let Some(return_value) = return_value {
            match self.etable.entries_mut().last_mut().unwrap().step_info {
                StepInfo::CallHost {
                    ref mut ret_val, ..
                } => {
                    *ret_val = Some(return_value.into());
                }
                StepInfo::ExternalHostCall {
                    ref mut value, sig, ..
                } if sig.is_ret() => {
                    *value = Some(return_value.into());
                }
                _ => unreachable!(),
            }
        }
    }
}
