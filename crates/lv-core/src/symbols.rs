//! 字符串驻留表：host / app / tag 等低基数字段统一驻留为 u32 符号，
//! 记录中只存 id，节省内存并加速等值比较。

use std::collections::HashMap;

pub const SYM_EMPTY: u32 = 0;

#[derive(Default)]
pub struct SymbolTable {
    map: HashMap<String, u32>,
    list: Vec<String>,
}

impl SymbolTable {
    pub fn new() -> Self {
        let mut t = Self::default();
        t.intern(""); // 保证 0 号是空串
        t
    }

    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = self.list.len() as u32;
        self.list.push(s.to_owned());
        self.map.insert(s.to_owned(), id);
        id
    }

    pub fn get(&self, id: u32) -> &str {
        self.list.get(id as usize).map(String::as_str).unwrap_or("")
    }

    pub fn lookup(&self, s: &str) -> Option<u32> {
        self.map.get(s).copied()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_dedup_and_roundtrip() {
        let mut t = SymbolTable::new();
        let a = t.intern("i2c");
        let b = t.intern("uart");
        let a2 = t.intern("i2c");
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(t.get(a), "i2c");
        assert_eq!(t.get(b), "uart");
        assert_eq!(t.get(SYM_EMPTY), "");
        assert_eq!(t.lookup("uart"), Some(b));
        assert_eq!(t.lookup("nope"), None);
    }
}
