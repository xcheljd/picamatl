//! Diagnostic: raw serialized size per indirect object (scan for "N G obj"
//! headers), bucketed and diffed between two files. Scratch tool.
use std::collections::HashMap;

fn obj_spans(raw: &[u8]) -> HashMap<u32, usize> {
    // find "\n<digits> <digits> obj" starts
    let mut starts: Vec<(usize, u32)> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\n' || i == 0 {
            let mut q = if raw[i] == b'\n' { i + 1 } else { i };
            let ds = q;
            while raw.get(q).is_some_and(|b| b.is_ascii_digit()) {
                q += 1;
            }
            if q > ds && raw.get(q) == Some(&b' ') {
                let id: u32 = std::str::from_utf8(&raw[ds..q])
                    .unwrap()
                    .parse()
                    .unwrap_or(0);
                let mut r = q + 1;
                while raw.get(r).is_some_and(|b| b.is_ascii_digit()) {
                    r += 1;
                }
                if raw.get(r..r + 4) == Some(b" obj") {
                    starts.push((ds, id));
                }
            }
        }
        i += 1;
    }
    let mut sizes = HashMap::new();
    for w in 0..starts.len() {
        let end = if w + 1 < starts.len() {
            starts[w + 1].0
        } else {
            raw.len()
        };
        sizes.insert(starts[w].1, end - starts[w].0);
    }
    sizes
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 3 {
        // dump mode: <file> dump <id>
        let raw = std::fs::read(&args[0]).unwrap();
        let id: u32 = args[2].parse().unwrap();
        let needle = format!("\n{id} 0 obj");
        let pos = raw
            .windows(needle.len())
            .position(|w| w == needle.as_bytes())
            .unwrap();
        let end = (pos + 400).min(raw.len());
        println!("{}", String::from_utf8_lossy(&raw[pos..end]).escape_debug());
        return;
    }
    let a = std::fs::read(&args[0]).unwrap();
    let b = std::fs::read(&args[1]).unwrap();
    let sa = obj_spans(&a);
    let sb = obj_spans(&b);
    let mut deltas: Vec<(i64, u32, usize, usize)> = Vec::new();
    for (&id, &za) in &sa {
        let zb = sb.get(&id).copied().unwrap_or(0);
        let d = zb as i64 - za as i64;
        if d != 0 {
            deltas.push((d, id, za, zb));
        }
    }
    deltas.sort();
    println!(
        "objects only in A: {}",
        sa.keys().filter(|k| !sb.contains_key(k)).count()
    );
    println!(
        "objects only in B: {}",
        sb.keys().filter(|k| !sa.contains_key(k)).count()
    );
    let total: i64 = deltas.iter().map(|d| d.0).sum();
    println!("total delta over shared ids: {total}");
    for d in deltas.iter().take(10) {
        println!("shrink: obj {} {} -> {} ({})", d.1, d.2, d.3, d.0);
    }
    for d in deltas.iter().rev().take(15) {
        println!("grow: obj {} {} -> {} (+{})", d.1, d.2, d.3, d.0);
    }
}
