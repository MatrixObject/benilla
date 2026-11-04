//! Throwaway probe: prints the `FactionTemplate.dbc` rows behind a player/creature reaction test.
use std::path::Path;

use benilla_formats::{load_faction_catalog, Chain};

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| {
        "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data\\dbc.MPQ".to_string()
    });
    let ids: Vec<u32> = std::env::args()
        .skip(2)
        .filter_map(|a| a.parse().ok())
        .collect();
    let mut chain = match Chain::open(Path::new(&dir)) {
        Ok(c) => c,
        Err(e) => {
            println!("chain: {e}");
            return;
        }
    };
    let bytes = match chain.read("DBFilesClient\\FactionTemplate.dbc") {
        Ok(b) => b,
        Err(e) => {
            println!("read: {e}");
            return;
        }
    };
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    let records = u32_at(4);
    let fields = u32_at(8);
    let record_size = u32_at(12) as usize;
    println!(
        "header: records={records} fields={fields} record_size={record_size} bytes={}",
        bytes.len()
    );
    let cat = match load_faction_catalog(&mut chain) {
        Ok(c) => c,
        Err(e) => {
            println!("catalog: {e}");
            return;
        }
    };
    for id in &ids {
        for r in 0..records as usize {
            let base = 20 + r * record_size;
            if u32_at(base) != *id {
                continue;
            }
            let cols: Vec<u32> = (0..fields as usize).map(|c| u32_at(base + c * 4)).collect();
            println!("raw {id}: {cols:?}");
            break;
        }
        let Some(t) = cat.template(*id) else {
            println!("tpl {id}: MISSING");
            continue;
        };
        println!(
            "tpl {id}: faction={} group={} friend_group={} enemy_group={} enemies={:?}",
            t.faction, t.group_mask, t.friend_group_mask, t.enemy_group_mask, t.enemies
        );
        println!(
            "  faction {}: name={:?} has_rep_slot={}",
            t.faction,
            cat.faction_name(t.faction),
            cat.reputation_faction(t.faction).is_some()
        );
    }
    for a in &ids {
        for b in &ids {
            if a < b {
                if let (Some(x), Some(y)) = (cat.template(*a), cat.template(*b)) {
                    println!(
                        "{a}->{b}: {:?}  {b}->{a}: {:?}",
                        x.reaction_toward(y),
                        y.reaction_toward(x)
                    );
                }
            }
        }
    }
    if let Some(want) = ids.first() {
        for id in &ids {
            println!(
                "faction {id}: name={:?} has_rep_slot={}",
                cat.faction_name(*id),
                cat.reputation_faction(*id).is_some()
            );
        }
    }
    if let Some(want) = ids.first() {
        let mut rows = Vec::new();
        for r in 0..records as usize {
            let base = 20 + r * record_size;
            if u32_at(base + 4) == *want {
                rows.push((
                    u32_at(base),
                    u32_at(base + 4),
                    u32_at(base + 12),
                    u32_at(base + 20),
                ));
            }
        }
        println!("templates (id, faction, group, enemy_group) whose faction == {want}: {rows:?}");
    }
}
