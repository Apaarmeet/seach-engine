use osmpbf::{Element, ElementReader};
use std::collections::HashMap;
fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap();
    let mut kinds: HashMap<String, u64> = HashMap::new();
    ElementReader::from_path(&path)?.for_each(|el| {
        let tags: Vec<(String,String)> = match &el {
            Element::Node(n) => n.tags().map(|(k,v)|(k.into(),v.into())).collect(),
            Element::DenseNode(n) => n.tags().map(|(k,v)|(k.into(),v.into())).collect(),
            _ => return,
        };
        let has_name = tags.iter().any(|(k,_)| k=="name");
        if let Some((_,v)) = tags.iter().find(|(k,_)| k=="place") {
            if has_name { *kinds.entry(v.clone()).or_default() += 1; }
        }
    })?;
    let mut v: Vec<_> = kinds.into_iter().collect();
    v.sort_by(|a,b| b.1.cmp(&a.1));
    let total: u64 = v.iter().map(|(_,c)| c).sum();
    println!("  named place nodes: {total}");
    for (k,c) in v.iter().take(9) { println!("    {k:<16} {c}"); }
    Ok(())
}
