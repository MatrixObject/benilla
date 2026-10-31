//! The wire `InventoryResult` to the `GlobalStrings` key of its red error line, a port of
//! `benilla-app`'s `ui_items::equip_error`.
//!
//! The reference maps every reason through the errorId table (`0x622794`, handler `0x5e3991`) and
//! shows it with `DisplayError` (`0x496720`), which resolves the key in the player's own
//! `GlobalStrings` — so a Chinese install says 背包已满, not an English line of ours. Positions are
//! vmangos `ItemDefines.h` at 5875 (stunned 37, dead 38, inventory full 50). Reason 0 is dropped
//! before the queue (`benilla_protocol::events`); reason 59 carries no 1.12 string and the
//! display's empty-string guard (`0x4945b4`) drops it.

/// A refusal reason's key, total like the reference's errorId table (`0x622794`).
pub(crate) fn equip_error_key(reason: u8) -> &'static str {
    match reason {
        1 => "ERR_CANT_EQUIP_LEVEL_I",
        2 => "ERR_CANT_EQUIP_SKILL",
        3 => "ERR_WRONG_SLOT",
        // BAG_FULL and its BAG_FULL3/4/6 aliases; 51 is not one.
        4 | 53 | 56 | 62 => "ERR_BAG_FULL",
        5 => "ERR_BAG_IN_BAG",
        6 => "ERR_TRADE_EQUIPPED_BAG",
        7 => "ERR_AMMO_ONLY",
        8 => "ERR_PROFICIENCY_NEEDED",
        9 | 12 | 18 => "ERR_NO_SLOT_AVAILABLE",
        10 | 11 => "ERR_CANT_EQUIP_EVER",
        13 => "ERR_2HANDED_EQUIPPED",
        14 => "ERR_2HSKILLNOTFOUND",
        15 | 16 => "ERR_WRONG_BAG_TYPE",
        17 => "ERR_ITEM_MAX_COUNT",
        19 | 55 => "ERR_CANT_STACK",
        20 => "ERR_NOT_EQUIPPABLE",
        21 => "ERR_CANT_SWAP",
        22 => "ERR_SLOT_EMPTY",
        23 | 54 => "ERR_ITEM_NOT_FOUND",
        24 => "ERR_DROP_BOUND_ITEM",
        25 => "ERR_OUT_OF_RANGE",
        26 => "ERR_TOO_FEW_TO_SPLIT",
        27 => "ERR_SPLIT_FAILED",
        28 => "ERR_SPELL_FAILED_REAGENTS_GENERIC",
        29 => "ERR_NOT_ENOUGH_MONEY",
        30 => "ERR_NOT_A_BAG",
        31 => "ERR_DESTROY_NONEMPTY_BAG",
        32 => "ERR_NOT_OWNER",
        33 => "ERR_ONLY_ONE_QUIVER",
        34 => "ERR_NO_BANK_SLOT",
        35 => "ERR_NO_BANK_HERE",
        36 => "ERR_ITEM_LOCKED",
        37 => "ERR_GENERIC_STUNNED",
        38 => "ERR_PLAYER_DEAD",
        39 => "ERR_CLIENT_LOCKED_OUT",
        40 => "ERR_INTERNAL_BAG_ERROR",
        // ERR_ONLY_ONE_BOLT's 1.12 string also says "quiver".
        41 => "ERR_ONLY_ONE_BOLT",
        42 => "ERR_ONLY_ONE_AMMO",
        43 => "ERR_CANT_WRAP_STACKABLE",
        44 => "ERR_CANT_WRAP_EQUIPPED",
        45 => "ERR_CANT_WRAP_WRAPPED",
        46 => "ERR_CANT_WRAP_BOUND",
        47 => "ERR_CANT_WRAP_UNIQUE",
        48 => "ERR_CANT_WRAP_BAGS",
        49 => "ERR_LOOT_GONE",
        50 => "ERR_INV_FULL",
        // EQUIP_ERR_BANK_FULL: the reference sets errorId 1 (`0x622661`), `ERR_BANK_FULL`, though
        // vmangos tags it `ERR_BAG_FULL`.
        51 => "ERR_BANK_FULL",
        52 | 57 => "ERR_VENDOR_SOLD_OUT",
        58 => "ERR_OBJECT_IS_BUSY",
        // EQUIP_ERR_NONE, the lock-clear sentinel: a real key with no 1.12 string.
        59 => "ERR_CANT_BE_DISENCHANTED",
        60 => "ERR_NOT_IN_COMBAT",
        61 => "ERR_NOT_WHILE_DISARMED",
        63 => "ERR_CANT_EQUIP_RANK",
        64 => "ERR_CANT_EQUIP_REPUTATION",
        65 => "ERR_TOO_MANY_SPECIAL_BAGS",
        66 => "ERR_LOOT_CANT_LOOT_THAT_NOW",
        // The reference's default past 66 (`0x62278d`): errorId 9, `ERR_BAG_FULL`.
        _ => "ERR_BAG_FULL",
    }
}

#[cfg(test)]
mod tests {
    use super::equip_error_key;

    /// Stunned and dead are 37 and 38 at 5875; 39 and 40 are the lock-out and the bag error.
    #[test]
    fn the_table_matches_the_5875_enum() {
        assert_eq!(equip_error_key(50), "ERR_INV_FULL");
        assert_eq!(equip_error_key(37), "ERR_GENERIC_STUNNED");
        assert_eq!(equip_error_key(38), "ERR_PLAYER_DEAD");
        assert_eq!(equip_error_key(39), "ERR_CLIENT_LOCKED_OUT");
        assert_eq!(equip_error_key(40), "ERR_INTERNAL_BAG_ERROR");
        assert_eq!(equip_error_key(1), "ERR_CANT_EQUIP_LEVEL_I");
    }

    /// 51 is the reference's `ERR_BANK_FULL`, and the BAG_FULL aliases are not.
    #[test]
    fn a_full_bank_says_bank_not_bag() {
        assert_eq!(equip_error_key(51), "ERR_BANK_FULL");
        for alias in [4u8, 53, 56, 62] {
            assert_eq!(equip_error_key(alias), "ERR_BAG_FULL", "reason {alias}");
        }
    }

    /// Past 66 the reference's default is `ERR_BAG_FULL`.
    #[test]
    fn an_unknown_reason_answers_the_default() {
        assert_eq!(equip_error_key(67), "ERR_BAG_FULL");
        assert_eq!(equip_error_key(255), "ERR_BAG_FULL");
    }
}
