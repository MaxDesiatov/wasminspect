use super::address::{FuncAddr, GlobalAddr};
use super::func::*;
use super::host::BuiltinPrintI32;
use super::module::*;
use super::stack::*;
use super::store::*;
use super::value::*;
use parity_wasm::elements::{InitExpr, Instruction, ValueType};

use std::convert::TryFrom;

#[derive(Debug)]
pub enum ExecError {
    Panic(String),
    NoCallFrame,
}

pub enum ExecSuccess {
    Next,
    End,
}

pub type ExecResult = Result<ExecSuccess, ExecError>;

#[derive(Debug)]
pub enum ReturnValError {
    TypeMismatchReturnValue(Value, ValueType),
    NoValue(ValueType),
    NoCallFrame,
}

pub type ReturnValResult = Result<Vec<Value>, ReturnValError>;

pub struct Executor {
    store: Store,
    pc: ProgramCounter,
    stack: Stack,
    last_ret_frame: Option<CallFrame>,
}

impl Executor {
    pub fn new(
        local_len: usize,
        func_addr: FuncAddr,
        initial_args: Vec<Value>,
        pc: ProgramCounter,
        store: Store,
    ) -> Self {
        let mut stack = Stack::default();
        let frame = CallFrame::new(func_addr, local_len, initial_args, None);
        let f = CallFrame::new(func_addr, local_len, vec![], None);
        stack.set_frame(frame);
        stack.push_label(Label::Return);
        Self {
            store,
            pc,
            stack,
            last_ret_frame: Some(f),
        }
    }

    pub fn peek_result(&self) -> ReturnValResult {
        let frame = match &self.last_ret_frame {
            Some(frame) => frame,
            None => return Err(ReturnValError::NoCallFrame),
        };
        let func = self.store.func(frame.func_addr);
        let return_ty = func.ty().return_type();
        // TODO: support multi value
        match (self.stack.peek_last_value(), return_ty) {
            (val, Some(ty)) => {
                if val.value_type() == ty {
                    return Ok(vec![val.clone()]);
                } else {
                    return Err(ReturnValError::TypeMismatchReturnValue(val.clone(), ty));
                }
            }
            (_, None) => return Ok(vec![]),
        }
    }

    pub fn current_func_insts(&self) -> &[Instruction] {
        let func = self.store.func(self.stack.current_func_addr());
        &func.defined().unwrap().code().instructions()
    }

    pub fn execute_step(&mut self) -> ExecResult {
        let func = self.store.func(self.pc.func_addr()).defined().unwrap();
        let module_index = func.module_index().clone();
        let inst = func.code().inst(self.pc.inst_index()).clone();
        return self.execute_inst(&inst, module_index);
    }

    fn execute_inst(&mut self, inst: &Instruction, module_index: ModuleIndex) -> ExecResult {
        self.pc.inc_inst_index();
        println!("{}", inst.clone());
        let result = match inst {
            Instruction::Unreachable => panic!(),
            Instruction::GetGlobal(index) => {
                let addr = GlobalAddr(module_index, *index as usize);
                let global = self.store.global(addr);
                self.stack.push_value(global.value());
                Ok(ExecSuccess::Next)
            }
            Instruction::SetGlobal(index) => {
                let addr = GlobalAddr(module_index, *index as usize);
                let value = self.stack.pop_value();
                self.store.set_global(addr, value);
                Ok(ExecSuccess::Next)
            }
            Instruction::SetLocal(index) => {
                let value = self.stack.pop_value();
                self.stack.set_local(*index as usize, value);
                Ok(ExecSuccess::Next)
            }
            Instruction::GetLocal(index) => {
                let value = self.stack.current_frame().local(*index as usize);
                self.stack.push_value(value);
                Ok(ExecSuccess::Next)
            }
            Instruction::I32Const(val) => {
                self.stack.push_value(Value::I32(*val));
                Ok(ExecSuccess::Next)
            }
            Instruction::I32Add => self.int_op::<i32, _>(|a, b| Value::I32(a + b)),
            Instruction::I32LtS => {
                self.int_op::<i32, _>(|a, b| Value::I32(if a < b { 1 } else { 0 }))
            }
            Instruction::I64Const(val) => {
                self.stack.push_value(Value::I64(*val));
                Ok(ExecSuccess::Next)
            }
            Instruction::F32Const(val) => {
                self.stack.push_value(Value::F32(f32::from_bits(*val)));
                Ok(ExecSuccess::Next)
            }
            Instruction::F64Const(val) => {
                self.stack.push_value(Value::F64(f64::from_bits(*val)));
                Ok(ExecSuccess::Next)
            }
            Instruction::Block(_) => {
                self.stack.push_label(Label::Block);
                Ok(ExecSuccess::Next)
            }
            Instruction::Loop(_) => {
                self.stack.push_label(Label::new_loop(self.pc.inst_index()));
                Ok(ExecSuccess::Next)
            }
            Instruction::If(_) => {
                self.stack.push_label(Label::If);
                let val: i32 = self.pop_as();
                if val == 0 {
                    let mut depth = 1;
                    loop {
                        let index = self.pc.inst_index().0 as usize;
                        match self.current_func_insts()[index] {
                            Instruction::End => depth -= 1,
                            Instruction::Block(_) => depth += 1,
                            Instruction::If(_) => depth += 1,
                            Instruction::Loop(_) => depth += 1,
                            Instruction::Else => {
                                if depth == 1 {
                                    self.pc.inc_inst_index();
                                    break;
                                }
                            }
                            _ => (),
                        }
                        if depth == 0 {
                            break;
                        }
                        self.pc.inc_inst_index();
                    }
                }
                Ok(ExecSuccess::Next)
            }
            Instruction::Else => {
                self.branch(0);
                Ok(ExecSuccess::Next)
            }
            Instruction::BrIf(depth) => {
                let val = self.stack.pop_value();
                if val != Value::I32(0) {
                    self.branch(*depth);
                }
                Ok(ExecSuccess::Next)
            }
            Instruction::Br(depth) => {
                self.branch(*depth);
                Ok(ExecSuccess::Next)
            }
            Instruction::Call(func_index) => {
                let frame = self.stack.current_frame();
                let addr = FuncAddr(frame.module_index(), *func_index as usize);
                self.invoke(addr)
            }
            Instruction::Return => {
                let frame = self.stack.current_frame();
                let func = self.store.func(frame.func_addr);
                let arity = func.ty().return_type().map(|_| 1).unwrap_or(0);
                let result = vec![];
                for _ in 0..arity {
                    result.push(self.stack.pop_value());
                }
                self.stack.pop_while(|v| match v {
                    StackValue::Activation(_) => false,
                    _ => true,
                });
                self.stack.pop_frame();
                for v in result {
                    self.stack.push_value(v);
                }

                if let Some(ret_pc) = frame.ret_pc {
                    self.pc = ret_pc;
                }
                Ok(ExecSuccess::Next)
            }
            Instruction::End => {
                if self.stack.current_frame_labels().is_empty() {
                    // When the end of a function is reached without a jump
                    let frame = self.stack.current_frame();
                    let func = self.store.func(frame.func_addr);
                    let arity = func.ty().return_type().map(|_| 1).unwrap_or(0);
                    let result = vec![];
                    for _ in 0..arity {
                        result.push(self.stack.pop_value());
                    }
                    self.stack.pop_frame();
                    for v in result {
                        self.stack.push_value(v);
                    }
                    if let Some(ret_pc) = frame.ret_pc {
                        self.pc = ret_pc;
                        Ok(ExecSuccess::Next)
                    } else {
                        Ok(ExecSuccess::End)
                    }
                } else {
                    // When the end of a block is reached without a jump
                    let results = self.stack.pop_while(|v| match v {
                        StackValue::Value(_) => true,
                        _ => false,
                    });
                    let label = &self.stack.pop_label();
                    for v in results {
                        self.stack.push_value(*v.as_value().unwrap());
                    }
                    match label {
                        Label::Loop(l) => self.pc.loop_jump(l),
                        _ => { /* nop */ }
                    }
                    Ok(ExecSuccess::Next)
                }
            }
            Instruction::Nop => Ok(ExecSuccess::Next),
            _ => {
                debug_assert!(false, format!("{} not supported yet", inst));
                ExecResult::Err(ExecError::Panic(format!("{} not supported yet", inst)))
            }
        };
        if self.stack.is_over_top_level() {
            return Ok(ExecSuccess::End);
        } else {
            return result;
        }
    }

    fn pop_as<T: TryFrom<Value>>(&mut self) -> T {
        let value = self.stack.pop_value();
        match T::try_from(value) {
            Ok(val) => val,
            Err(_) => panic!(),
        }
    }

    fn branch(&mut self, depth: u32) {
        self.stack.pop_labels(depth as usize);
        match self.stack.peek_last_label() {
            Label::Loop(loop_label) => self.pc.loop_jump(loop_label),
            Label::If | Label::Block => {
                let mut depth = depth + 1;
                loop {
                    let index = self.pc.inst_index().0 as usize;
                    match self.current_func_insts()[index] {
                        Instruction::End => depth -= 1,
                        Instruction::Block(_) => depth += 1,
                        Instruction::If(_) => depth += 1,
                        Instruction::Loop(_) => depth += 1,
                        _ => (),
                    }
                    if depth == 0 {
                        break;
                    }
                    self.pc.inc_inst_index();
                }
            }
            Label::Return => panic!(),
        }
    }

    fn int_op<T: TryFrom<Value>, F: Fn(T, T) -> Value>(&mut self, f: F) -> ExecResult {
        let rhs = self.pop_as();
        let lhs = self.pop_as();
        self.stack.push_value(f(lhs, rhs));
        Ok(ExecSuccess::Next)
    }

    fn invoke(&self, addr: FuncAddr) -> ExecResult {
        let func = self.store.func(addr);
        let mut args = Vec::new();
        for _ in func.ty().params() {
            args.push(self.stack.pop_value());
        }
        match func {
            FunctionInstance::Defined(defined) => {
                let pc = ProgramCounter::new(addr, InstIndex::zero());
                args.reverse();
                let frame = CallFrame::new_from_func(addr, &defined, args, Some(self.pc));
                self.stack.set_frame(frame);
                self.stack.push_label(Label::Return);
                self.pc = pc;
                Ok(ExecSuccess::Next)
            }
            FunctionInstance::Host(host) => match &host.field_name()[..] {
                "print_i32" => {
                    BuiltinPrintI32::dispatch(&args);
                    Ok(ExecSuccess::Next)
                }
                _ => panic!(),
            },
        }
    }
}

pub fn eval_const_expr(init_expr: &InitExpr) -> Value {
    let inst = &init_expr.code()[0];
    match *inst {
        Instruction::I32Const(val) => Value::I32(val),
        Instruction::I64Const(val) => Value::I64(val),
        Instruction::F32Const(val) => Value::F32(f32::from_bits(val)),
        Instruction::F64Const(val) => Value::F64(f64::from_bits(val)),
        Instruction::GetGlobal(_) => panic!(),
        _ => panic!("Unsupported init_expr {}", inst),
    }
}
