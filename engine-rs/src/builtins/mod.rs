use crate::value::Val;
use crate::vm::Vm;

pub(crate) mod core_collections;
pub(crate) mod data_gui;
pub(crate) mod math_text_fs;
pub(crate) mod science_ml;
pub(crate) mod sys_net_conc_ffi;

impl Vm {
    pub(crate) fn call_builtin(&mut self, n: &str, a: Vec<Val>) -> Result<Option<Val>, String> {
        if let Some(v) = self.call_builtin_core_collections(n, a.clone())? {
            return Ok(Some(v));
        }
        if let Some(v) = self.call_builtin_math_text_fs(n, a.clone())? {
            return Ok(Some(v));
        }
        if let Some(v) = self.call_builtin_science_ml(n, a.clone())? {
            return Ok(Some(v));
        }
        if let Some(v) = self.call_builtin_data_gui(n, a.clone())? {
            return Ok(Some(v));
        }
        if let Some(v) = self.call_builtin_sys_net_conc_ffi(n, a.clone())? {
            return Ok(Some(v));
        }
        if let Some(v) = crate::universal_arch::call_builtin_universal_arch(self, n, a)? {
            return Ok(Some(v));
        }
        Ok(None)
    }
}
