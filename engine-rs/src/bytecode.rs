// Bytecode operation set and the function, class and object definitions the
// compiler emits and the VM executes.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::HashMap;

use crate::value::Val;

#[derive(Clone, Debug)]
pub(crate) enum Op {
    Const(Val),
    Load(String),
    Define(String, bool, Option<String>),
    Store(String),
    Pop,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Neg,
    Not,
    And,
    Or,
    Index,
    GetMember(String),
    SetMember(String),
    Call(String, usize),
    CallMethod(String, usize),
    FunctionRef(String),
    Def(
        String,
        Vec<(String, Option<String>)>,
        Option<String>,
        bool,
        Vec<Op>,
    ),
    DefClass(
        String,
        Option<String>,
        Vec<(
            String,
            Vec<(String, Option<String>)>,
            Option<String>,
            bool,
            Vec<Op>,
        )>,
    ),
    Return,
    Yield,
    PushHandler(usize, String),
    PopHandler,
    Throw,
    JumpIfFalse(usize),
    Jump(usize),
    Halt,
}

#[derive(Clone, Debug)]
pub(crate) struct FunctionDef {
    pub(crate) params: Vec<(String, Option<String>)>,
    pub(crate) return_type: Option<String>,
    pub(crate) is_async: bool,
    pub(crate) code: Vec<Op>,
    pub(crate) captures: HashMap<String, Val>,
    pub(crate) capture_types: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ClassDef {
    pub(crate) parent: Option<String>,
    pub(crate) methods: HashMap<String, FunctionDef>,
}

#[derive(Clone, Debug)]
pub(crate) struct ObjectData {
    pub(crate) class_name: String,
    pub(crate) fields: HashMap<String, Val>,
}
