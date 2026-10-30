//! The chain's twelve `Blizzard_*` LoadOnDemand addons as the AddOn registry sees them.
//!
//! On a 1.12 install each of those folders holds only a `.pub`: the files live in the patch
//! archives, and the reference's startup scan (`0x51c760`) reads each archive's listfile for
//! them before `UI_Init` walks the toc. Without the registry rows `UIParentLoadAddOn` finds no
//! addon to load, and the dialogs that live in one (`Blizzard_TalentUI`'s `TalentFrame`, and the
//! auction, craft, inspect, macro, raid, trainer and tradeskill windows with it) never exist.
//!
//! The rows are [`AddOnInfo`]s built from each toc exactly as `benilla-app`'s discovery builds
//! them (`crates/benilla-app/src/ui_script/addons.rs`, `info_from_toc`), and the reader resolves
//! `Interface/AddOns/<Name>/<file>` off the same patch chain.

use std::path::Path;

use benilla_formats::Chain;
use benilla_ui::script::{AddOnInfo, AddonSecurity, UiScript};
use benilla_ui::toc::Toc;

/// The reference's twelve `Blizzard_*` LoadOnDemand addons, in the scan's order.
const BLIZZARD_ADDONS: [&str; 12] = [
    "Blizzard_AuctionUI",
    "Blizzard_BattlefieldMinimap",
    "Blizzard_BindingUI",
    "Blizzard_CombatText",
    "Blizzard_CraftUI",
    "Blizzard_GMSurveyUI",
    "Blizzard_InspectUI",
    "Blizzard_MacroUI",
    "Blizzard_RaidUI",
    "Blizzard_TalentUI",
    "Blizzard_TradeSkillUI",
    "Blizzard_TrainerUI",
];

/// Seat the registry rows and the chain reader, returning how many rows registered.
pub(crate) fn register(script: &mut UiScript, chain: &Chain, data_dir: &str) -> usize {
    let locale = chain.locale().index();
    let addons: Vec<AddOnInfo> = BLIZZARD_ADDONS
        .iter()
        .filter_map(|name| {
            let bytes = chain
                .read(&format!(r"Interface\AddOns\{name}\{name}.toc"))
                .ok()?;
            let toc = Toc::parse(&benilla_ui::source::decode(&bytes));
            // Only a LoadOnDemand toc is a registry row: the startup walk does not read the
            // chain, so an addon there without the flag is never loaded anyway.
            toc.load_on_demand()
                .then(|| info_from_toc(name, &toc, locale))
        })
        .collect();
    let count = addons.len();
    script.register_addons(addons, None, None, None);
    // The reader opens its own chain lazily: `Chain` owns the archives and cannot be shared with
    // the load's handle, and a load that never asks for an addon pays nothing.
    let folder = data_dir.to_string();
    let opened = std::cell::RefCell::new(None::<Chain>);
    script.set_addon_chain_reader(Box::new(move |req: &str| {
        let mut chain = opened.borrow_mut();
        if chain.is_none() {
            *chain = Chain::open(Path::new(&folder)).ok();
        }
        chain.as_ref()?.read(&req.replace('/', "\\")).ok()
    }));
    count
}

/// One registry row, `benilla-app`'s `info_from_toc` over the same toc fields.
fn info_from_toc(name: &str, toc: &Toc, locale: usize) -> AddOnInfo {
    AddOnInfo {
        name: name.to_owned(),
        title: toc.metadata_value(name, "Title", locale),
        notes: toc.metadata_value(name, "Notes", locale),
        // Only `SMSG_ADDON_INFO` writes the url (`0x51db6a`); the toc has no such line.
        url: None,
        claims_secure: toc.secure(),
        // The reference reads `<Name>.toc.sig` with Blizzard's key; benilla has no key, so an
        // addon read from the player's own chain that claims `## Secure` stands as signature-ok.
        signature_ok: toc.secure(),
        installed_revision: toc.revision(),
        // INSECURE until the server's reply says otherwise (`0x5205b6`).
        security: AddonSecurity::Insecure,
        corrupt: false,
        new_version: false,
        default_state: toc.default_state(),
        load_on_demand: toc.load_on_demand(),
        dependencies: toc.dependencies().into_iter().map(str::to_owned).collect(),
        optional_dependencies: toc
            .optional_dependencies()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        load_with: toc.load_with().into_iter().map(str::to_owned).collect(),
        dependents: Vec::new(), // filled by `register_addons`
        directives: toc.metadata(name, locale),
        files: toc.files.clone(),
        saved_variables: toc
            .saved_variables()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        saved_variables_per_character: toc
            .saved_variables_per_character()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        hidden: false,
        enabled: true,
        saved_enabled: true,
        loaded: false,
        interface: toc.interface_version(),
        chain: true,
    }
}
