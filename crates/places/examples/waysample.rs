//! Show concrete places that the nodes-only ingest misses.
use osmpbf::{Element, ElementReader};
fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap();
    let mut samples: Vec<(String, String, String)> = Vec::new();
    ElementReader::from_path(&path)?.for_each(|el| {
        let Element::Way(w) = &el else { return };
        if samples.len() >= 400 { return }
        let tags: Vec<(String,String)> = w.tags().map(|(k,v)|(k.into(),v.into())).collect();
        let Some(name) = tags.iter().find(|(k,_)| k=="name").map(|(_,v)| v.clone()) else { return };
        let pairs: Vec<(&str,&str)> = tags.iter().map(|(k,v)|(k.as_str(),v.as_str())).collect();
        let Some((cat,_)) = places::poi::categorise(pairs) else { return };
        // Only interesting categories, to make the point vivid.
        if matches!(cat.as_str(), "mall"|"hospital"|"university"|"college"|"supermarket"|"stadium"|"museum") {
            samples.push((name, cat, format!("way/{}", w.refs().count())));
        }
    })?;
    println!("examples of places mapped as POLYGONS (currently skipped):");
    for (n,c,r) in samples.iter().take(12) {
        println!("  {c:<12} {n:<44} {r} nodes");
    }
    Ok(())
}
