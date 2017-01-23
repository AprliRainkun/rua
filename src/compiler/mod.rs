use self::opcodes::*;
use self::symbol_table::*;
use self::resource_allocator::*;
use self::types::*;
use parser::types::*;
use lexer::tokens::FlagType;
use std::collections::HashMap;
use std::ptr;

pub mod symbol_table;
pub mod resource_allocator;
pub mod opcodes;
pub mod types;

#[derive(Debug)]
pub struct CodeGen {
    symbol_table: ScopedSymbolTableBuilder,
    flag_to_op: HashMap<FlagType, OpName>,
    root_function: FunctionChunk,
}

// public interface
impl CodeGen {
    pub fn new() -> CodeGen {
        CodeGen {
            symbol_table: ScopedSymbolTableBuilder::new(),
            flag_to_op: get_opflag_opname_map(),
            root_function: FunctionChunk::new(),
        }
    }

    pub fn compile(&mut self, ast: &Node) -> Result<(), CompileError> {
        self.visit_unit(ast)
    }
}

// visit method
impl CodeGen {
    fn visit_unit(&mut self, node: &Node) -> Result<(), CompileError> {
        // TODO: add header
        // root block should not have retstat
        // is_vararg (always 2 for top level function )
        self.visit_function(node, None, &vec![], true)
            .map(|mut func| self.root_function = func.prototype)
    }
    /// visit function body, ret: upvalue_num, prototype
    /// assuming scope is newly initiated
    fn visit_function(&mut self,
                      node: &Node,
                      parent_alloc: Option<&mut ResourceAlloc>,
                      paras: &Vec<Name>,
                      is_vararg: bool)
                      -> Result<FunctionPrototype, CompileError> {

        let mut res_alloc = ResourceAlloc::new().parent(if parent_alloc.is_some() {
            parent_alloc.unwrap() as *mut ResourceAlloc
        } else {
            ptr::null_mut()
        });
        let mut instructions = Vec::<OpMode>::new();
        let mut func_chunk = FunctionChunk::new();
        //  define parameters and reserve registers
        for name in paras {
            let pos = res_alloc.reg_alloc.push(Some(name));
            self.symbol_table.define_local(name, pos);
        }
        //  visit body instuctions
        try!(self.visit_block(node, &mut res_alloc, &mut instructions));
        // add a return, may be redundant
        CodeGen::emit_iABx(&mut instructions, OpName::RETURN, 0, 1);
        // number of upvalues
        func_chunk.upvalue_num = res_alloc.upvalue_alloc.size() as Usize;
        // number of parameters
        func_chunk.para_num = paras.len() as Usize;
        func_chunk.is_vararg = is_vararg;
        // maximum stack size ( number of register used )
        func_chunk.stack_size = res_alloc.reg_alloc.size();
        // list of instructions
        //    size
        func_chunk.ins_len = instructions.len() as Usize;
        //    instructions
        func_chunk.instructions = instructions;
        // list of constants
        func_chunk.constants = res_alloc.const_alloc.dump();
        // list of function prototypes
        func_chunk.funclist_len = res_alloc.function_alloc.size() as Usize;
        func_chunk.function_prototypes = res_alloc.function_alloc.get_function_prototypes();
        Ok(FunctionPrototype::new(func_chunk, res_alloc.upvalue_alloc.into_list()))
    }

    /// ret: number of returned
    /// None means no return stat
    fn visit_block(&mut self,
                   node: &Node,
                   res_alloc: &mut ResourceAlloc,
                   instructions: &mut Vec<OpMode>)
                   -> Result<(), CompileError> {
        if let Node::Block(Block { ref stats, ref ret }) = *node {
            for stat in stats {
                self.visit_stat(stat, res_alloc, instructions)?;
            }
            // if ret statement exists
            if let &Some(ref ret_exprs) = ret {
                self.visit_stat(&Stat::Ret(ret_exprs.clone()), res_alloc, instructions)?;
            }
            Ok(())
        } else {
            panic!("Block should be ensured by parser");
        }
    }

    /// ret:
    fn visit_stat(&mut self,
                  stat: &Stat,
                  res_alloc: &mut ResourceAlloc,
                  instructions: &mut Vec<OpMode>)
                  -> Result<(), CompileError> {
        match *stat {
            // could be global or local`
            Stat::Assign(ref varlist, ref exprlist) => {
                // visit each expr, and get result register
                //       trim varlist and exprlist into equal length and
                //       return new varlist and exprlist
                //       this method will generate load nill instruction
                //       and have a special handler for functioncall
                //       loadnill should be performed at the end
                let (varlist, exprlist) =
                    self.adjust_list(varlist, exprlist, res_alloc, instructions)?;
                let reg_list = try!(self.visit_exprlist(&exprlist, res_alloc, instructions));
                for (var, expr) in varlist.into_iter().zip(reg_list.into_iter()) {
                    match var {
                        Var::Name(ref name) => {
                            // lookup , confirm if symbol is already defined
                            match self.symbol_table.lookup(name) {
                                Some((SymbolScope::Global, _)) |
                                None => {
                                    let const_pos = self.prepare_global_value(name, res_alloc); /* add name to const list and define global symbol */
                                    CodeGen::emit_iABx(instructions,
                                                       OpName::SETGLOBAL,
                                                       expr.1, // only need reg pos
                                                       const_pos);
                                }
                                Some((SymbolScope::Local, pos)) => {
                                    CodeGen::emit_iABx(instructions, OpName::MOVE, pos, expr.1);
                                }
                                Some((SymbolScope::UpValue(_), _)) => unimplemented!(),
                            }
                        }
                        _ => unimplemented!(),
                    }
                }
                Ok(())
            }
            // bind to new local
            Stat::AssignLocal(ref namelist, ref exprlist) => {
                let (namelist, exprlist) =
                    self.adjust_list(namelist, exprlist, res_alloc, instructions)?;
                let reg_list = try!(self.visit_exprlist(&exprlist, res_alloc, instructions));
                for (ref name, (is_temp, expr_reg)) in namelist.into_iter()
                    .zip(reg_list.into_iter()) {
                    if is_temp {
                        res_alloc.reg_alloc.push_set(name, expr_reg);
                        self.symbol_table.define_local(name, expr_reg);
                    } else {
                        let pos = res_alloc.reg_alloc.push(Some(name));
                        self.symbol_table.define_local(name, pos);
                        CodeGen::emit_iABx(instructions, OpName::MOVE, pos, expr_reg);
                    }
                }
                Ok(())
            }
            Stat::Ret(ref exprlist) => {
                // first: allocate a chunk of conjective registers
                let reg_list =
                    (0..exprlist.len()).map(|_| res_alloc.reg_alloc.push(None)).collect::<Vec<_>>();
                // if is void return, start_register is not needed
                let ret_num = reg_list.len(); // save moved value
                let start_register = if ret_num > 0 { reg_list[0] } else { 0 };
                // second: visit each expr with expect return register
                for (expr, reg) in exprlist.into_iter().zip(reg_list.into_iter()) {
                    try!(self.visit_expr(expr, res_alloc, instructions, Some(Expect::Reg(reg))));
                }
                // return statement
                // if B == 1, no expr returned
                // if B >= 1 return R(start_register) .. R(start_register + B - 2)
                CodeGen::emit_iABx(instructions,
                                   OpName::RETURN,
                                   start_register,
                                   (ret_num + 1) as u32);
                Ok(())
            }
            _ => unimplemented!(),
        }
    }

    /// ret: (is_temp, a register hold the value)
    /// expect: where the result should be stored or how many result should be returned
    fn visit_expr(&mut self,
                  expr: &Expr,
                  res_alloc: &mut ResourceAlloc,
                  instructions: &mut Vec<OpMode>,
                  expect: Option<Expect>)
                  -> Result<(bool, Usize), CompileError> {
        match *expr {
            Expr::Num(num) => {
                let const_pos = res_alloc.const_alloc.push(ConstType::Real(num));
                let reg = if let Some(expect) = extract_expect_reg(expect)? {
                    expect
                } else {
                    res_alloc.reg_alloc.push(None)
                };
                CodeGen::emit_iABx(instructions, OpName::LOADK, reg, const_pos);
                Ok((true, reg))
            }
            Expr::Boole(value) => {
                let bit = if value { 1 } else { 0 };
                let reg = if let Some(expect) = extract_expect_reg(expect)? {
                    expect
                } else {
                    res_alloc.reg_alloc.push(None)
                };
                CodeGen::emit_iABC(instructions, OpName::LOADBOOL, reg, bit, 0);
                Ok((true, reg))
            }
            Expr::BinOp(flag, ref left, ref right) => {
                // use left register as result register
                // TODO: ignore left associative to generate optimized code
                match flag {
                    FlagType::Plus | FlagType::Minus | FlagType::Mul | FlagType::Div => {
                        let (is_temp, left_reg) =
                            try!(self.visit_expr(left, res_alloc, instructions, None));
                        let (_, right_reg) =
                            try!(self.visit_expr(right, res_alloc, instructions, None));
                        let op = self.flag_to_op.get(&flag).expect("BinOp not defined").clone();
                        // destructive op only generate for temp register
                        let (result_is_temp, result_reg) = if let Some(expect) =
                                                                  extract_expect_reg(expect)? {
                            (false, expect)
                        } else {
                            if is_temp {
                                (true, left_reg)
                            } else {
                                (true, res_alloc.reg_alloc.push(None))
                            }
                        };
                        CodeGen::emit_iABC(instructions, op, result_reg, left_reg, right_reg);
                        Ok((result_is_temp, result_reg))
                    }
                    _ => self.visit_logic_arith(expr, res_alloc, instructions, expect),

                }
            }
            Expr::Var(ref var) => {
                self.visit_var(var, res_alloc, instructions, extract_expect_reg(expect)?)
            }
            Expr::FunctionDef((ref namelist, is_vararg), ref function_body) => {
                self.symbol_table.initialize_scope();
                //  child function prototype should be wrapped in another scope
                let function_prototype =
                    try!(self.visit_function(function_body, Some(res_alloc), namelist, is_vararg));
                self.symbol_table.finalize_scope();
                //  push function prototype in function list
                let func_pos = res_alloc.function_alloc.push(function_prototype.prototype);
                //  temporary register for function
                let reg = if let Some(expect) = extract_expect_reg(expect)? {
                    expect
                } else {
                    res_alloc.reg_alloc.push(None)
                };
                CodeGen::emit_iABx(instructions, OpName::CLOSURE, reg, func_pos);
                //  generate virtual move instructions
                //  helping vm to manage upvalue
                for (is_immidiate, pos_in_vl, pos_in_parent) in function_prototype.upvalue_list {
                    //  move: pass the variable in current lexical scope to closure
                    if is_immidiate {
                        CodeGen::emit_iABx(instructions, OpName::MOVE, pos_in_vl, pos_in_parent);
                    } else {
                        // getupval: pass upvalue to the closure
                        CodeGen::emit_iABx(instructions,
                                           OpName::GETUPVAL,
                                           pos_in_vl,
                                           pos_in_parent);
                    }
                }
                Ok((true, reg))
            }
            Expr::FunctionCall(ref expr, ref args, is_vararg) => {
                let central_reg = self.visit_function_call(expr,
                                         args,
                                         is_vararg,
                                         RetExpect::Num(1),
                                         res_alloc,
                                         instructions)?;
                Ok((true, central_reg))
            }
            Expr::UnaryOp(op, ref left) => {
                match op{
                    FlagType::Minus => unimplemented!(),
                    FlagType::Plus => self.visit_expr(left, res_alloc, instructions, expect),
                    _ => self.visit_logic_arith(expr, res_alloc, instructions, expect),
                }
            }
            _ => unimplemented!(),
        }
    }

    fn visit_logic_arith(&mut self,
                         expr: &Expr,
                         res_alloc: &mut ResourceAlloc,
                         instruction: &mut Vec<OpMode>,
                         expect: Option<Expect>)
                         -> Result<(bool, u32), CompileError> {
        let result_reg = if let Some(expect) = extract_expect_reg(expect)? {
            expect
        } else {
            res_alloc.reg_alloc.push(None)
        };

        let true_label = res_alloc.label_alloc.new_label();
        let false_label = res_alloc.label_alloc.new_label();
        let mut raw = self.visit_boolean_expr(expr, res_alloc, true_label, false_label, true)?;
        raw.push(OpMode::Label(false_label));
        CodeGen::emit_iABC(&mut raw, OpName::LOADBOOL, result_reg, 0, 1);
        raw.push(OpMode::Label(true_label));
        CodeGen::emit_iABC(&mut raw, OpName::LOADBOOL, result_reg, 1, 0);
        instruction.append(&mut raw.remove_label());
        Ok((true, result_reg))
    }

    fn visit_boolean_expr(&mut self,
                          expr: &Expr,
                          res_alloc: &mut ResourceAlloc,
                          true_br: Label,
                          false_br: Label,
                          fall_through: bool)
                          -> Result<Vec<OpMode>, CompileError> {
        match *expr {
            Expr::BinOp(op, ref left, ref right) => {
                match op {
                    FlagType::OR => {
                        let label_for_right = res_alloc.label_alloc.new_label();
                        let mut left_raw =
                            self.visit_boolean_expr(left,
                                                    res_alloc,
                                                    true_br,
                                                    label_for_right,
                                                    true)?;
                        let mut right_raw =
                            self.visit_boolean_expr(right, res_alloc, true_br, false_br, fall_through)?;
                        // merge
                        left_raw.push(OpMode::Label(label_for_right));
                        left_raw.append(&mut right_raw);
                        Ok(left_raw)
                    }
                    FlagType::AND => {
                        let label_for_right = res_alloc.label_alloc.new_label();
                        let mut left_raw =
                            self.visit_boolean_expr(left,
                                                    res_alloc,
                                                    label_for_right,
                                                    false_br,
                                                    false)?;
                        let mut right_raw =
                            self.visit_boolean_expr(right, res_alloc, true_br, false_br, fall_through)?;
                        left_raw.push(OpMode::Label(label_for_right));
                        left_raw.append(&mut right_raw);
                        Ok(left_raw)
                    }
                    FlagType::LESS | FlagType::LEQ | FlagType::GREATER | FlagType::GEQ |
                    FlagType::EQ | FlagType::NEQ => {
                        let mut raw = vec![];
                        let (_, left_reg) = self.visit_expr(left, res_alloc, &mut raw, None)?;
                        let (_, right_reg) = self.visit_expr(right, res_alloc, &mut raw, None)?;
                        let (op_name, test_bool) = match op {
                            FlagType::LESS => (OpName::LT, true),
                            FlagType::LEQ => (OpName::LE, true),
                            FlagType::GREATER => (OpName::LE, false),
                            FlagType::GEQ => (OpName::LT, false),
                            FlagType::EQ => (OpName::EQ, true),
                            FlagType::NEQ => (OpName::EQ, false),
                            _ => panic!("should not appear"),
                        };
                        // adjust code arrangement according to fall_through
                        let (test_int, path) = if fall_through{
                            (test_bool as u32, true_br)
                        } else {
                            ((!test_bool) as u32, false_br)
                        };
                        CodeGen::emit_iABC(&mut raw, op_name, test_int, left_reg, right_reg);
                        CodeGen::emit_iAsBx(&mut raw, OpName::JMP, 0, path);
                        // CodeGen::emit_iAsBx(&mut raw, OpName::JMP, 0, false_br);
                        Ok(raw)
                    }
                    _ => panic!("expression not accept as boolean"),
                }
            }
            Expr::UnaryOp(op, ref left) => {
                match op {
                    FlagType::Not => {
                        self.visit_boolean_expr(left, res_alloc, false_br, true_br, !fall_through)
                    }
                    _ => panic!("expression not accept as boolean"),
                }
            }
            Expr::Var(ref var) => {
                let mut raw = vec![];
                let (_, reg) = self.visit_var(var, res_alloc, &mut raw, None)?;
                if fall_through == true {
                    // fall to true path
                    CodeGen::emit_iABx(&mut raw, OpName::TEST, reg, 1);
                    CodeGen::emit_iAsBx(&mut raw, OpName::JMP, 0, true_br);
                    // CodeGen::emit_iAsBx(&mut raw, OpName::JMP, 0, false_br);
                } else {
                    // fall to false path
                    CodeGen::emit_iABx(&mut raw, OpName::TEST, reg, 0);
                    CodeGen::emit_iAsBx(&mut raw, OpName::JMP, 0, false_br);
                    // CodeGen::emit_iAsBx(&mut raw, OpName::JMP, 0, true_br);
                }
                Ok(raw)
            }
            Expr::Boole(value) => {
                let mut raw = vec![];
                if value {
                    CodeGen::emit_iAsBx(&mut raw, OpName::JMP, 0, true_br);
                } else {
                    CodeGen::emit_iAsBx(&mut raw, OpName::JMP, 0, false_br);
                }
                Ok(raw)
            }
            _ => panic!("expression not accept"),
        }
    }

    fn visit_exprlist(&mut self,
                      exprlist: &Vec<Box<Expr>>,
                      res_alloc: &mut ResourceAlloc,
                      instructions: &mut Vec<OpMode>)
                      -> Result<Vec<(bool, u32)>, CompileError> {
        let reg_list = exprlist.into_iter()
            .map(|expr| self.visit_expr(expr, res_alloc, instructions, None))
            .filter(|r| r.is_ok())
            .map(|r| r.unwrap())
            .collect::<Vec<_>>();
        if reg_list.len() != exprlist.len() {
            Err(CompileError::SyntexError)
        } else {
            Ok(reg_list)
        }
    }

    /// ret: (is_temp, register saves the varible)
    fn visit_var(&mut self,
                 var: &Var,
                 res_alloc: &mut ResourceAlloc,
                 instructions: &mut Vec<OpMode>,
                 expect_reg: Option<u32>)
                 -> Result<(bool, Usize), CompileError> {
        match *var {
            Var::Name(ref name) => {
                let (scope, pos) =
                    try!(self.symbol_table.lookup(name).ok_or(CompileError::UndefinedSymbol));
                match scope {
                    SymbolScope::Global => {
                        let const_pos = res_alloc.const_alloc.push(ConstType::Str(name.clone()));
                        let reg = if let Some(expect) = expect_reg {
                            expect
                        } else {
                            res_alloc.reg_alloc.push(None)
                        };
                        CodeGen::emit_iABx(instructions, OpName::GETGLOBAL, reg, const_pos);
                        Ok((true, reg))
                    }
                    SymbolScope::UpValue(depth) => {
                        let immidiate_upvalue_pos =
                            unsafe { res_alloc.propagate_upvalue(name, pos, depth) };
                        let reg = if let Some(expect) = expect_reg {
                            expect
                        } else {
                            res_alloc.reg_alloc.push(None)
                        };
                        // todo: optimize, reduce register number
                        CodeGen::emit_iABx(instructions,
                                           OpName::GETUPVAL,
                                           reg,
                                           immidiate_upvalue_pos);
                        Ok((true, reg))
                    }
                    SymbolScope::Local => {
                        if let Some(expect) = expect_reg {
                            if expect != pos {
                                CodeGen::emit_iABx(instructions, OpName::MOVE, expect, pos);
                                Ok((false, expect)) // caller-provided register is viewed as none temp
                            } else {
                                Ok((false, expect))
                            }
                        } else {
                            // no expected register provided
                            Ok((false, pos))
                        }
                    }
                }
            }
            Var::Reg(reg) => Ok((true, reg)),
            Var::PrefixExp(ref expr) => unimplemented!(),
        }
    }

    fn visit_function_call(&mut self,
                           expr: &Expr,
                           args: &Vec<Box<Expr>>,
                           is_vararg: bool,
                           expect_ret: RetExpect,
                           res_alloc: &mut ResourceAlloc,
                           instructions: &mut Vec<OpMode>)
                           -> Result<u32, CompileError> {
        // todo: vararg
        // get function name
        let func_pos = res_alloc.reg_alloc.push(None);
        self.visit_expr(expr, res_alloc, instructions, Some(Expect::Reg(func_pos)))?;
        let ret_field: u32;
        let arg_field: u32;

        match expect_ret {
            RetExpect::Num(ret_num) => {
                // allocate register
                for _ in 0..ret_num {
                    res_alloc.reg_alloc.push(None);
                }
                ret_field = ret_num + 1;
            }
            RetExpect::Indeterminate => {
                ret_field = 0;
            }
        }

        // in case of underflow
        if args.len() == 0 {
            arg_field = 1;
        } else {
            let mut args_reg = func_pos + 1;
            if let Expr::FunctionCall(ref expr, ref args, is_vararg) = *args[args.len() - 1] {
                for i in 0..(args.len() - 1) {
                    // make sure the args are continous
                    self.visit_expr(&args[i],
                                    res_alloc,
                                    instructions,
                                    Some(Expect::Reg(args_reg)))?;
                    args_reg += 1;
                }
                self.visit_function_call(expr,
                                         args,
                                         is_vararg,
                                         RetExpect::Indeterminate,
                                         res_alloc,
                                         instructions)?;
                arg_field = 0; // Indeterminate arg number
            } else {
                for expr in args {
                    self.visit_expr(expr, res_alloc, instructions, Some(Expect::Reg(args_reg)))?;
                    args_reg += 1;
                }
                arg_field = args.len() as u32 + 1;
            }
        }
        CodeGen::emit_iABC(instructions, OpName::CALL, func_pos, arg_field, ret_field);
        Ok(func_pos)
    }

    fn adjust_list<T: Clone>(&mut self,
                             varlist: &Vec<T>,
                             exprlist: &Vec<Box<Expr>>,
                             res_alloc: &mut ResourceAlloc,
                             instructions: &mut Vec<OpMode>)
                             -> Result<(Vec<T>, Vec<Box<Expr>>), CompileError> {
        // balanced
        if varlist.len() == exprlist.len() {
            return Ok((varlist.clone(), exprlist.clone()));
        }
        // imbalanced & trancate
        else if varlist.len() < exprlist.len() {
            // discard resisual expressions
            let remain = varlist.len();
            let mut truncated = exprlist.clone();
            truncated.truncate(remain);
            return Ok((varlist.clone(), truncated));
        }
        // imbalanced & one function call
        else {
            if exprlist.len() == 1 {
                if let Expr::FunctionCall(ref expr, ref args, is_vararg) = *exprlist[0] {
                    let central_reg = self.visit_function_call(expr,
                                             args,
                                             is_vararg,
                                             RetExpect::Num(varlist.len() as u32),
                                             res_alloc,
                                             instructions)?;
                    let expr_regs = (central_reg..(central_reg + varlist.len() as u32))
                        .map(|reg| Box::new(Expr::Var(Var::Reg(reg))))
                        .collect();
                    return Ok((varlist.clone(), expr_regs));
                }
            }
            // imbalanced & loadnill
            let num = (varlist.len() - exprlist.len()) as u32;
            let start_reg = res_alloc.reg_alloc.push(None);
            let mut extended = exprlist.clone();
            extended.push(Box::new(Expr::Var(Var::Reg(start_reg))));
            for _ in 1..num {
                let reg = res_alloc.reg_alloc.push(None);
                extended.push(Box::new(Expr::Var(Var::Reg(reg))));
            }
            CodeGen::emit_iABx(instructions, OpName::LOADNIL, start_reg, num - 1);
            return Ok((varlist.clone(), extended));
        }
    }
}

impl CodeGen {
    /// allocate name in const
    /// and define in global scope
    /// avoiding duplication included
    fn prepare_global_value(&mut self, name: &str, res_alloc: &mut ResourceAlloc) -> Usize {
        let pos = res_alloc.const_alloc.push(ConstType::Str(name.to_string()));
        self.symbol_table.define_global(name);
        pos
    }

    /// put iABx instruction in bytecode vector
    #[allow(non_snake_case)]
    fn emit_iABx(instructions: &mut Vec<OpMode>, op: OpName, A: u32, Bx: u32) {
        instructions.push(OpMode::iABx(op, A, Bx));
    }

    #[allow(non_snake_case)]
    fn emit_iABC(instructions: &mut Vec<OpMode>, op: OpName, A: u32, B: u32, C: u32) {
        instructions.push(OpMode::iABC(op, A, B, C));
    }

    #[allow(non_snake_case)]
    fn emit_iAsBx(instructions: &mut Vec<OpMode>, op: OpName, A: u32, sBx: i32) {
        instructions.push(OpMode::iAsBx(op, A, sBx));
    }
}

// #[cfg(test)]
pub mod tests {
    use super::*;
    use parser::Parser;
    use std::str::Chars;
    #[test]
    fn global_arith() {
        let ast = Parser::<Chars>::ast_from_text(&String::from("\
            a, b = 2.5 , 2 * 4
            local c = (a + b) / 10.5
        "))
            .unwrap();

        let mut compiler = CodeGen::new();
        assert_eq!(compiler.compile(&ast), Ok(()));
    }

    #[test]
    fn function_def() {
        let ast = Parser::<Chars>::ast_from_text(&String::from("\
            local a = 2
            func = function()
                local b = 3 
                return a + b, b + a
            end
        "))
            .unwrap();

        let mut compiler = CodeGen::new();
        assert_eq!(compiler.compile(&ast), Ok(()));
    }

    #[test]
    pub fn function_call() {
        let ast = Parser::<Chars>::ast_from_text(&String::from("\
            local a = 2
            func = function(para)
                return a + para, 0
            end
            local b, c = func(1, 2)
            local d = b + c
        "))
            .unwrap();
        let mut compiler = CodeGen::new();
        assert_eq!(compiler.compile(&ast), Ok(()));
        // println!("{:?}", compiler.root_function);
    }
    // #[test]
    pub fn boolean() {
        let ast = Parser::<Chars>::ast_from_text(&String::from("\
            local a, b = true, false
            local c = not ( 2 <= 3 or a == b )
        "))
            .unwrap();
        let mut compiler = CodeGen::new();
        assert_eq!(compiler.compile(&ast), Ok(()));
        println!("{:?}", compiler.root_function);
    }
}