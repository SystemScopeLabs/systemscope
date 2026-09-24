//! Checks on the pinned `riscv-tests` checkout, run by `cargo xtask rv32-fixtures` right
//! after a build: the selection must account for the whole upstream `rv32ui` list, and
//! each selected wrapper must include the body the manifest names.

use std::fs;
use std::path::Path;

use crate::{EXCLUDED, SELECTED};

/// The test names in the `rv32ui_sc_tests` variable of `isa/rv32ui/Makefrag`.
pub fn makefrag_tests(makefrag: &str) -> Result<Vec<String>, String> {
    let mut lines = makefrag.lines();
    lines
        .by_ref()
        .find(|l| l.trim_start().starts_with("rv32ui_sc_tests"))
        .ok_or("no rv32ui_sc_tests in the Makefrag")?;
    let mut names = Vec::new();
    for line in lines {
        let line = line.trim();
        let (words, continued) = match line.strip_suffix('\\') {
            Some(words) => (words, true),
            None => (line, false),
        };
        names.extend(words.split_whitespace().map(str::to_owned));
        if !continued {
            break;
        }
    }
    Ok(names)
}

/// Checks the `riscv-tests` checkout at `riscv_tests`:
///
/// - the upstream `rv32ui` list is exactly [`SELECTED`] and the [`EXCLUDED`] names, each
///   once;
/// - every selected wrapper `isa/rv32ui/<t>.S` includes `../rv64ui/<t>.S`, the body the
///   manifest records.
pub fn check(riscv_tests: &Path) -> Result<(), Vec<String>> {
    let isa = riscv_tests.join("isa");
    let read =
        |rel: &str| fs::read_to_string(isa.join(rel)).map_err(|e| vec![format!("isa/{rel}: {e}")]);
    let mut upstream = makefrag_tests(&read("rv32ui/Makefrag")?).map_err(|e| vec![e])?;
    upstream.sort();
    let mut ours: Vec<String> = SELECTED
        .iter()
        .chain(EXCLUDED.iter().map(|(name, _)| name))
        .map(|&n| n.to_owned())
        .collect();
    ours.sort();
    let mut errors = Vec::new();
    if upstream != ours {
        errors.push(format!(
            "the upstream rv32ui list {upstream:?} is not the selected and excluded tests \
             {ours:?}"
        ));
    }
    for name in SELECTED {
        let wrapper = read(&format!("rv32ui/{name}.S"))?;
        let include = format!("#include \"../rv64ui/{name}.S\"");
        if !wrapper.lines().any(|l| l.trim() == include) {
            errors.push(format!("isa/rv32ui/{name}.S does not {include}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_makefrag_list_is_read_up_to_its_last_continuation() {
        let text = "# c\n\nrv32ui_sc_tests = \\\n\tsimple \\\n\tadd addi \\\n\txori \\\n\n\
                    rv32ui_p_tests = $(addprefix rv32ui-p-, $(rv32ui_sc_tests))\n";
        assert_eq!(
            makefrag_tests(text),
            Ok(vec![
                "simple".to_owned(),
                "add".to_owned(),
                "addi".to_owned(),
                "xori".to_owned()
            ])
        );
        assert!(makefrag_tests("nothing").is_err());
    }
}
