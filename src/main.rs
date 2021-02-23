use std::env;
use std::io::Write;

use getopts::Options;
use std::collections::BTreeMap;

use pdb::{FallibleIterator, SymbolData, PDB, LineProgram, AddressMap};


/// File and line number mapping for an instruction address.
#[derive(Clone, Debug)]
pub struct LineInfo {
    /// The instruction address relative to the image base (load address).
    pub address: u64,
    /// Total code size covered by this line record.
    pub size: Option<u64>,
    /// File name and path.
    pub file: String,
    /// Absolute line number starting at 1. Zero means no line number.
    pub line: u64,
}

fn collect_lines<I>(
    mut line_iter: I,
    program: &LineProgram,
    address_map: &AddressMap,
    string_table: &pdb::StringTable,
) -> Result<Vec<LineInfo>, pdb::Error>
where
    I: FallibleIterator<Item = pdb::LineInfo, Error = pdb::Error>
{

    let mut lines = Vec::new();
    while let Some(line_info) = line_iter.next()? {
        let rva = match line_info.offset.to_rva(&address_map) {
            Some(rva) => u64::from(rva.0),
            None => continue,
        };

        let file_info = program.get_file_info(line_info.file_index)?;
        lines.push(LineInfo {
            address: rva,
            size: line_info.length.map(u64::from),
            file: file_info.name.to_string_lossy(string_table).unwrap().to_string(),
            line: line_info.line_start.into(),
        });
    }

    Ok(lines)
}

fn dump_pdb(filename: &str, targets: Vec<u32>) -> pdb::Result<()> {
    let file = std::fs::File::open(filename)?;
    let mut pdb = PDB::open(file)?;

    let address_map = pdb.address_map()?;
    let string_table = pdb.string_table();
    let string_table = match string_table {
        Ok(string_table) => string_table,
        _ => panic!("No string table in .pdb.")
    };

    let dbi = pdb.debug_information()?;
    let ipi = pdb.id_information()?;

    let mut modules = dbi.modules()?;
    while let Some(module) = modules.next()? {

        let info = match pdb.module_info(&module)? {
            Some(info) => info,
            None => {
                //println!("  no module info");
                continue;
            }
        };

        let inlinees: BTreeMap<_, _> = info.inlinees()?.map(|i| Ok((i.index(), i))).collect()?;

        let program = info.line_program()?;
        let mut symbols = info.symbols()?;

        let mut depth = 0;
        let mut inc_next = false;

        let mut proc_offsets = Vec::new();

        while let Some(symbol) = symbols.next()? {

            if inc_next {
                depth += 1;
            }

            inc_next = symbol.starts_scope();
            if symbol.ends_scope() {
                depth -= 1;

                if proc_offsets.last().map_or(false, |&(d, _)| d >= depth) {
                    proc_offsets.pop();
                }
            }

            match symbol.parse() {
                Ok(SymbolData::Procedure(proc)) => {
                    proc_offsets.push((depth, proc.offset));
                    
                    match proc.offset.to_rva(&address_map) {
                        Some(start) => {
                            for target in &targets {
                                if start.0 <= *target && *target < start.0 + proc.len {

                                    let mut lines = program.lines_at_offset(proc.offset).peekable();
                                    while let Some(line_info) = lines.next()? {
                                        let rva = line_info.offset.to_rva(&address_map).expect("invalid rva");
                                        let file_info = program.get_file_info(line_info.file_index)?;
                                        let file_name = file_info.name.to_string_lossy(&string_table)?;
                                        match lines.peek()? {
                                            Some(info) => {
                                                if rva.0 <= *target && info.offset.to_rva(&address_map).expect("invalid rva").0 > *target {
                                                    println!("{:#x} {} ({}:{})", target, proc.name, file_name, line_info.line_start);
                                                    break;
                                                }
                                            }
                                            _ => println!("{:#x} {} ({}:{})", target, proc.name, file_name, line_info.line_start),
                                        };
                                    }
                                }
                            }
                        }
                        _ => {}
                    }

                }
                Ok(SymbolData::InlineSite(site)) => {
                    let parent_offset = proc_offsets
                        .last()
                        .map(|&(_, offset)| offset).unwrap();

                    // We can assume that inlinees will be listed in the inlinee table. If missing,
                    // skip silently instead of erroring out. Missing a single inline function is
                    // more acceptable in such a case than halting iteration completely.
                    if let Some(inlinee) = inlinees.get(&site.inlinee) {
                        // println!("Found inline parent_offset {:?} {:?} {:?}", parent_offset.to_rva(&address_map), site, inlinee);
                        let line_iter = inlinee.lines(parent_offset, &site);
                        let lines = collect_lines(line_iter, &program, &address_map, &string_table)?;
                        for l in lines {
                            for target in &targets {
                                if l.address <= (*target).into() && l.address + l.size.unwrap() > (*target).into() {
                                    let mut found_inlinee = false;
                                    for i in ipi.iter().iterator() {
                                        if let Ok(i) = i {
                                            if i.index() == site.inlinee {
                                                match i.parse() {
                                                    Ok(pdb::IdData::Function(i)) => {
                                                        println!("{:#x} {} ({}:{})", target, i.name, l.file, l.line);
                                                        found_inlinee = true;
                                                        break;
                                                    }
                                                    _ => {}
                                                }
                                                // func_name = p.
                                                break;
                                            }
                                        }
                                    }

                                    if !found_inlinee { println!("{:#x} {} ({}:{})", target, "unknown_inline_function", l.file, l.line); }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut opts = Options::new();
    opts.optflag("h", "help", "print this help menu");
    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => panic!(f.to_string()),
    };

    let (filename, addresses_str) = if matches.free.len() >= 2 {
        (&matches.free[0], &matches.free[1..])
    } else {
        //print_usage(&program, opts);
        println!("specify path to a PDB");
        return;
    };

    let mut addresses: Vec<u32> = Vec::new();
    for address_str in addresses_str {
        let address = u32::from_str_radix(address_str.trim_start_matches("0x"), 16).unwrap();
        addresses.push(address);
    }

    match dump_pdb(&filename, addresses) {
        Ok(_) => {}
        Err(e) => {
            writeln!(&mut std::io::stderr(), "error dumping PDB: {}", e).expect("stderr write");
        }
    }
}
