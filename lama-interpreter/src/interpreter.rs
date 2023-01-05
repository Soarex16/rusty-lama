use std::rc::Rc;

use lama_bc::bytecode::*;

use crate::{
    builtin::{Environment, RustEnvironment},
    call_stack::CallStack,
    error::InterpreterError,
    scope::Scope,
    stack::Stack,
    value::Value,
};

pub struct Interpreter<'a> {
    ip: usize,
    bf: &'a ByteFile<'a>,
    stack: Stack,
    call_stack: CallStack,
    globals: Scope,
    builtins: Box<dyn Environment>,
}

impl Interpreter<'_> {
    pub fn new<'a>(bf: &'a ByteFile) -> Interpreter<'a> {
        let mut stack = Stack::new();
        stack.push(Value::Empty); // placeholders
        stack.push(Value::Empty); // for
        stack.push(Value::ReturnAddress(bf.code.len())); // main

        Interpreter {
            globals: Scope::new(bf.global_area_size),
            call_stack: CallStack::new(),
            builtins: Box::new(RustEnvironment),
            stack,
            ip: 0,
            bf,
        }
    }

    fn begin(&mut self, nargs: usize, nlocals: usize) -> Result<(), InterpreterError> {
        let return_address = InstructionPtr(self.stack.pop()?.unwrap_return_addr()?);
        self.call_stack
            .begin(self.stack.take(nargs)?, nlocals, return_address);
        Ok(())
    }

    fn end(&mut self) -> Result<(), InterpreterError> {
        let ret = self.call_stack.end()?;
        self.jump(&ret)
    }

    fn call(&mut self, ptr: &InstructionPtr) -> Result<(), InterpreterError> {
        self.stack.push(Value::ReturnAddress(self.ip));
        self.jump(ptr)
    }

    fn lookup(&self, loc: &Location) -> Result<Value, InterpreterError> {
        match loc {
            Location::Global(l) => self.globals.lookup(*l as usize),
            Location::Closure(_) => Err(InterpreterError::Unknown(
                "Closures are not supported".to_string(),
            )),
            l => self.call_stack.top()?.lookup(l),
        }
    }

    fn set(&mut self, loc: &Location, val: Value) -> Result<(), InterpreterError> {
        match loc {
            Location::Closure(_) => Err(InterpreterError::Unknown(
                "Closures are not supported".to_string(),
            )),
            Location::Global(l) => self.globals.set(*l as usize, val),
            l => self.call_stack.top_mut()?.set(l, val),
        }
    }

    pub fn run(&mut self) -> Result<(), InterpreterError> {
        while self.ip < self.bf.code.len() {
            let opcode = &self.bf.code[self.ip];

            // println!("ip: {}", self.ip);
            // println!("stack: {:?}", self.stack);
            // println!("opcode: {:?}", opcode);

            match opcode {
                OpCode::CONST(x) => self.stack.push(Value::Int(*x)),
                OpCode::BINOP(op) => {
                    let rhs = self.stack.pop()?;
                    let lhs = self.stack.pop()?;
                    self.stack.push(self.eval_bin_op(op, lhs, rhs)?)
                }

                OpCode::JMP(ptr) => {
                    self.jump(ptr)?;
                    continue;
                }
                OpCode::CJMP(cond, ptr) => {
                    let v = self.stack.pop()?.unwrap_int()?;
                    match cond {
                        JumpCondition::Zero => {
                            if v == 0 {
                                self.jump(ptr)?;
                                continue;
                            }
                        }
                        JumpCondition::NotZero => {
                            if v != 0 {
                                self.jump(ptr)?;
                                continue;
                            }
                        }
                    }
                }

                OpCode::BEGIN { nargs, nlocals } => {
                    self.begin(*nargs as usize, *nlocals as usize)?
                }
                OpCode::END => {
                    self.end()?;
                }
                OpCode::CALL { ptr, nargs: _ } => {
                    self.call(ptr)?;
                    continue;
                }
                OpCode::FAIL(line, _) => {
                    let x = self.stack.pop()?;
                    return Err(InterpreterError::Failure(format!(
                        "matching value {} failure at {}",
                        x, line
                    )));
                }

                OpCode::DROP => self.stack.drop()?,
                OpCode::DUP => self.stack.dup()?,
                OpCode::SWAP => self.stack.swap()?,

                OpCode::LD(loc) => self.stack.push(self.lookup(loc)?),
                OpCode::ST(loc) => {
                    let val = self.stack.pop()?;
                    self.stack.push(val.clone());
                    self.set(loc, val)?
                }

                OpCode::STI => {
                    let val = self.stack.pop()?;
                    let loc = self.stack.pop()?.unwrap_ref()?;
                    self.set(&loc, val)?
                }
                OpCode::SEXP { tag, size } => {
                    let mut vals = self.stack.take(*size as usize)?;
                    vals.reverse();
                    let tag_label = self
                        .bf
                        .string(tag)
                        .map_err(|_| InterpreterError::InvalidString(*tag))?
                        .to_owned();
                    self.stack.push(Value::Sexp(*tag, tag_label, Rc::new(vals)))
                }
                OpCode::STRING(s) => {
                    let str = self
                        .bf
                        .string(s)
                        .map_err(|_| InterpreterError::InvalidString(*s))?
                        .to_owned();
                    self.stack.push(Value::String(str))
                }
                OpCode::LDA(loc) => self.stack.push(Value::Ref(*loc)),
                OpCode::STA => todo!(),
                OpCode::ELEM => {
                    let idx = self.stack.pop()?.unwrap_int()? as usize;
                    let val = self.stack.pop()?;
                    let item = match val {
                        Value::Sexp(_, _, ref items) => items
                            .get(idx)
                            .ok_or_else(|| InterpreterError::IndexOutOfRange(idx)),
                        Value::Array(ref items) => items
                            .get(idx)
                            .ok_or_else(|| InterpreterError::IndexOutOfRange(idx)),
                        _ => Err(InterpreterError::UnexpectedValue {
                            expected: "array or sexp".to_string(),
                            found: val.to_string(),
                        }),
                    }?;
                    self.stack.push(item.clone());
                }

                OpCode::BUILTIN(b) => {
                    let res = self.builtins.eval(*b, &mut self.stack)?;
                    self.stack.push(res);
                }

                OpCode::TAG { tag, size } => {
                    let matches = match self.stack.pop()? {
                        Value::Sexp(t, _, items) if t == *tag && items.len() == *size as usize => 1,
                        _ => 0,
                    };
                    self.stack.push(Value::Int(matches))
                }
                OpCode::ARRAY(size) => {
                    let matches = match self.stack.pop()? {
                        Value::Array(items) if items.len() == *size as usize => 1,
                        _ => 0,
                    };
                    self.stack.push(Value::Int(matches))
                }
                OpCode::PATT(_) => todo!(),

                OpCode::CBEGIN { .. } => {
                    return Err(InterpreterError::UnsupportedInstruction(
                        "CBEGIN".to_string(),
                    ))
                }
                OpCode::CLOSURE { .. } => {
                    return Err(InterpreterError::UnsupportedInstruction(
                        "CLOSURE".to_string(),
                    ))
                }
                OpCode::CALLC { .. } => {
                    return Err(InterpreterError::UnsupportedInstruction(
                        "CALLC".to_string(),
                    ))
                }

                OpCode::LINE(_) => (),
                OpCode::RET => (), // unused
            }
            self.ip += 1;
        }
        Ok(())
    }

    fn jump(&mut self, ptr: &InstructionPtr) -> Result<(), InterpreterError> {
        let new_ip = ptr.0 as usize;
        if new_ip > self.bf.code.len() {
            Err(InterpreterError::InvalidInstructionPtr(new_ip))?;
        }

        self.ip = new_ip;
        Ok(())
    }

    fn eval_bin_op(&self, op: &BinOp, lhs: Value, rhs: Value) -> Result<Value, InterpreterError> {
        let l = lhs.unwrap_int()?;
        let r = rhs.unwrap_int()?;

        let res = match op {
            BinOp::Plus => l + r,
            BinOp::Minus => l - r,
            BinOp::Mul => l * r,
            BinOp::Div => l / r,
            BinOp::Mod => l % r,
            BinOp::Lt => {
                if l < r {
                    1
                } else {
                    0
                }
            }
            BinOp::LtEq => {
                if l <= r {
                    1
                } else {
                    0
                }
            }
            BinOp::Gt => {
                if l > r {
                    1
                } else {
                    0
                }
            }
            BinOp::GtEq => {
                if l >= r {
                    1
                } else {
                    0
                }
            }
            BinOp::Eq => {
                if l == r {
                    1
                } else {
                    0
                }
            }
            BinOp::Neq => {
                if l != r {
                    1
                } else {
                    0
                }
            }
            BinOp::And => {
                if l & r != 0 {
                    1
                } else {
                    0
                }
            }
            BinOp::Or => {
                if l | r != 0 {
                    1
                } else {
                    0
                }
            }
        };

        Ok(Value::Int(res))
    }
}