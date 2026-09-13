//! Quantify the coverage gap from indexing nodes only.
//!
//! Ways (building polygons) are skipped by the ingest because deriving a
//! centroid needs geometry resolution. This counts what that costs.
use osmpbf::{Element, ElementReader};

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: waygap <file.osm.pbf>");
    let reader = ElementReader::from_path(&path)?;

    let (mut n_ok, mut w_ok) = (0u64, 0u64);
    let (mut n_chain, mut w_chain) = (0u64, 0u64);
    const CHAINS: [&str; 6] = ["starbucks", "domino", "mcdonald", "kfc", "cafe coffee day", "reliance"];

    reader.for_each(|el| {
        let tags: Vec<(String, String)> = match &el {
            Element::Node(n) => n.tags().map(|(k, v)| (k.into(), v.into())).collect(),
            Element::DenseNode(n) => n.tags().map(|(k, v)| (k.into(), v.into())).collect(),
            Element::Way(w) => w.tags().map(|(k, v)| (k.into(), v.into())).collect(),
            _ => return,
        };
        let name = tags.iter().find(|(k, _)| k == "name" || k == "brand").map(|(_, v)| v.clone());
        let Some(name) = name else { return };
        let pairs: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        if places::poi::categorise(pairs).is_none() {
            return;
        }
        let lower = name.to_lowercase();
        let is_chain = CHAINS.iter().any(|c| lower.contains(c));
        match el {
            Element::Way(_) => {
                w_ok += 1;
                if is_chain { w_chain += 1; }
            }
            _ => {
                n_ok += 1;
                if is_chain { n_chain += 1; }
            }
        }
    })?;

    let total = n_ok + w_ok;
    println!("named, categorisable places in the extract:");
    println!("  nodes (indexed)     {n_ok:>8}");
    println!("  ways  (SKIPPED)     {w_ok:>8}");
    println!("  total               {total:>8}");
    println!("  coverage            {:.1}%  -> {:.1}% missing", 
        n_ok as f64 / total as f64 * 100.0, w_ok as f64 / total as f64 * 100.0);
    println!();
    println!("well-known chains specifically:");
    println!("  nodes (indexed)     {n_chain:>8}");
    println!("  ways  (SKIPPED)     {w_chain:>8}");
    if n_chain + w_chain > 0 {
        println!("  missing             {:.1}%", w_chain as f64 / (n_chain + w_chain) as f64 * 100.0);
    }
    Ok(())
}
