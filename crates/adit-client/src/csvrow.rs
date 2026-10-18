// SPDX-License-Identifier: Apache-2.0
//! Results rows in the schema of `bench/schema/<name>.csv`, read at runtime so the binaries and
//! the Go schema check cannot drift apart.

use std::collections::BTreeMap;

pub struct Row {
    cols: Vec<String>,
    vals: BTreeMap<String, String>,
}

fn esc(v: &str) -> String {
    if v.contains([',', '"', '\n']) { format!("\"{}\"", v.replace('"', "\"\"")) } else { v.to_string() }
}

impl Row {
    pub fn from_schema(path: &str) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let header = text.lines().find(|l| !l.starts_with('#') && !l.trim().is_empty()).unwrap_or_default();
        Ok(Self { cols: header.split(',').map(str::to_string).collect(), vals: BTreeMap::new() })
    }

    /// Set a column; unknown columns are an error so a typo cannot silently drop a measurement.
    pub fn set(&mut self, k: &str, v: impl ToString) -> Result<(), String> {
        if !self.cols.iter().any(|c| c == k) {
            return Err(format!("column {k:?} is not in the schema"));
        }
        self.vals.insert(k.to_string(), v.to_string());
        Ok(())
    }

    pub fn header(&self) -> String {
        self.cols.join(",")
    }

    pub fn line(&self) -> String {
        self.cols.iter().map(|c| esc(self.vals.get(c).map(String::as_str).unwrap_or(""))).collect::<Vec<_>>().join(",")
    }
}
