//! Offline startup adapter: owns the game-core instance without introducing transport behavior.

use std::path::Path;

use benilla_game_core::{
    Appearance, CharacterId, CombatUpdate, GameCore, HostileState, SaveError, WorldPosition,
    decode_profile, decode_snapshot,
};
use benilla_protocol::{CharAction, CharCreateReq, EntityKind, ObjectFields, SessionEvent};
use bevy::prelude::*;

use crate::local_state;
use crate::net::{CharActionResultMessage, CharListMessage, CharRequest, OfflineChannels};

#[derive(Resource, Default)]
pub(crate) struct OfflineGameCore(pub(crate) GameCore);

#[derive(Resource)]
struct OfflinePersistence {
    writable: bool,
}

pub(crate) struct OfflineGamePlugin;

impl Plugin for OfflineGamePlugin {
    fn build(&self, app: &mut App) {
        let (core, writable) = match load_core() {
            Ok(Some(core)) => (core, true),
            Ok(None) => (GameCore::default(), true),
            Err(error) => {
                error!("offline save load failed; persistence disabled: {error}");
                (GameCore::default(), false)
            }
        };
        app.insert_resource(OfflineGameCore(core))
            .insert_resource(OfflinePersistence { writable })
            .add_systems(Startup, publish_local_roster)
            .add_systems(
                Update,
                drive_local_requests
                    .in_set(benilla_world::schedule::WorldStage::Net)
                    .before(crate::net::apply_net_updates),
            )
            .add_systems(
                Update,
                drive_offline_gameplay
                    .in_set(benilla_world::schedule::WorldStage::Net)
                    .after(drive_local_requests)
                    .before(crate::net::apply_net_updates),
            )
            .add_systems(
                Update,
                update_local_player.after(benilla_world::schedule::WorldStage::Input),
            );
        let smoke = std::env::var_os("BENILLA_OFFLINE_SMOKE").is_some();
        if std::env::var_os("BENILLA_OFFLINE_BOOT_CHECK").is_some() && !smoke {
            app.add_systems(Update, exit_after_roster_boot);
        }
        if smoke {
            app.add_systems(
                Update,
                enter_offline_smoke_character.before(drive_local_requests),
            )
            .add_systems(
                Update,
                drive_offline_smoke_walk.before(crate::bindings::BindingSet),
            )
            .add_systems(
                Update,
                drive_offline_smoke_cast.before(drive_offline_gameplay),
            )
            .add_systems(Update, exit_after_world_smoke);
        }
    }
}

fn publish_local_roster(
    core: Res<OfflineGameCore>,
    catalog: Option<Res<crate::entities::CharCreate>>,
    mut roster: MessageWriter<CharListMessage>,
) {
    send_roster(&core.0, catalog.as_deref(), &mut roster);
}

fn send_roster(
    core: &GameCore,
    catalog: Option<&crate::entities::CharCreate>,
    roster: &mut MessageWriter<CharListMessage>,
) {
    let characters = core
        .character_ids()
        .filter_map(|id| core.snapshot(id))
        .map(|snapshot| {
            let p = snapshot.position;
            let mut equipment = [benilla_protocol::CharEnumItem::default(); 19];
            if let Some(catalog) = catalog {
                for item in catalog.0.start_outfit(
                    snapshot.appearance.race,
                    snapshot.appearance.class,
                    snapshot.appearance.gender,
                ) {
                    if let Some(slot) = inventory_type_to_roster_slot(item.inv_type) {
                        equipment[slot] = benilla_protocol::CharEnumItem {
                            display_id: item.display_id,
                            inventory_type: item.inv_type,
                        };
                    }
                }
            }
            benilla_protocol::Character {
                guid: snapshot.id.presentation_guid(),
                name: snapshot.name.clone(),
                race: snapshot.appearance.race,
                class: snapshot.appearance.class,
                gender: snapshot.appearance.gender,
                skin: snapshot.appearance.skin,
                face: snapshot.appearance.face,
                hair_style: snapshot.appearance.hair_style,
                hair_color: snapshot.appearance.hair_color,
                facial_hair: snapshot.appearance.facial_hair,
                level: snapshot.level,
                zone: 0,
                map: p.map_id,
                position: benilla_protocol::wire::Vector3d {
                    x: p.x,
                    y: p.y,
                    z: p.z,
                },
                flags: 0,
                equipment,
                pet_display_id: 0,
                pet_level: 0,
                pet_family: 0,
            }
        })
        .collect();
    roster.write(CharListMessage {
        characters,
        realm: None,
    });
}

fn load_core() -> Result<Option<GameCore>, String> {
    let profile_path = local_state::offline_profile_path()
        .ok_or_else(|| "local state root is unavailable".to_string())?;
    let Some(profile) = read_checked_document(&profile_path, decode_profile)? else {
        return Ok(None);
    };
    let mut snapshots = Vec::new();
    for (id, _) in profile.entries() {
        let path = local_state::offline_character_path(id.as_str())
            .ok_or_else(|| format!("invalid character id {}", id.as_str()))?;
        let snapshot = read_checked_document(&path, decode_snapshot)?
            .ok_or_else(|| format!("missing character snapshot for {}", id.as_str()))?;
        snapshots.push(snapshot);
    }
    GameCore::restore(profile, snapshots)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn read_checked_document<T>(
    path: &Path,
    decode: impl Fn(&str) -> Result<T, SaveError>,
) -> Result<Option<T>, String> {
    let backup = local_state::backup_path(path);
    let primary_error = match std::fs::read_to_string(path) {
        Ok(source) => match decode(&source) {
            Ok(document) => return Ok(Some(document)),
            Err(error) => Some(error.to_string()),
        },
        Err(error) if path.exists() => Some(error.to_string()),
        Err(_) => None,
    };

    let backup_source = match std::fs::read_to_string(&backup) {
        Ok(source) => match decode(&source) {
            Ok(document) => {
                warn!("offline save recovered from {}", backup.display());
                local_state::write_atomic_bytes(path, source.as_bytes()).map_err(|error| {
                    format!("repairing {} from backup: {error}", path.display())
                })?;
                return Ok(Some(document));
            }
            Err(error) => Some(error.to_string()),
        },
        Err(error) if backup.exists() => Some(error.to_string()),
        Err(_) => None,
    };

    match (primary_error, backup_source) {
        (None, None) => Ok(None),
        (Some(error), None) | (None, Some(error)) => Err(format!("{}: {error}", path.display())),
        (Some(primary), Some(backup)) => Err(format!(
            "{} is invalid ({primary}); backup is invalid ({backup})",
            path.display()
        )),
    }
}

fn save_core(core: &GameCore) -> Result<(), String> {
    let profile_path = local_state::offline_profile_path()
        .ok_or_else(|| "local state root is unavailable".to_string())?;
    for id in core.character_ids() {
        let path = local_state::offline_character_path(id.as_str())
            .ok_or_else(|| format!("invalid character id {}", id.as_str()))?;
        let bytes = core
            .encode_snapshot(id)
            .map_err(|error| error.to_string())?;
        local_state::write_atomic_bytes_with_backup(&path, bytes.as_bytes())
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
    }
    let bytes = core.encode_profile().map_err(|error| error.to_string())?;
    local_state::write_atomic_bytes_with_backup(&profile_path, bytes.as_bytes())
        .map_err(|error| format!("writing {}: {error}", profile_path.display()))
}

fn drive_local_requests(
    channels: Res<OfflineChannels>,
    mut core: ResMut<OfflineGameCore>,
    mut persistence: ResMut<OfflinePersistence>,
    catalog: Option<Res<crate::entities::CharCreate>>,
    creatures: Option<Res<crate::entities::Creatures>>,
    factions: Option<Res<crate::target::Factions>>,
    mut names: ResMut<crate::names::NameCache>,
    mut roster: MessageWriter<CharListMessage>,
    mut action_results: MessageWriter<CharActionResultMessage>,
) {
    while let Ok(request) = channels.picks.try_recv() {
        match request {
            CharRequest::Enter(guid) => {
                let Some(id) = core.0.id_for_presentation_guid(guid).cloned() else {
                    warn!("offline roster: refused unknown character {guid:#x}");
                    continue;
                };
                if core.0.select_character(&id).is_err() {
                    continue;
                }
                match core.0.enter_world() {
                    Ok(snapshot) => {
                        emit_world_entry(
                            &channels,
                            &snapshot,
                            core.0.hostiles(),
                            catalog.as_deref(),
                            creatures.as_deref(),
                            factions.as_deref(),
                            &mut names,
                        );
                        save_after_command(&core.0, &mut persistence);
                    }
                    Err(error) => warn!("offline roster: enter failed: {error}"),
                }
            }
            CharRequest::Delete(guid) => {
                let result = core
                    .0
                    .id_for_presentation_guid(guid)
                    .cloned()
                    .ok_or(benilla_game_core::CoreError::CharacterNotFound)
                    .and_then(|id| core.0.delete_character(&id));
                let code = if result.is_ok() {
                    save_after_command(&core.0, &mut persistence);
                    send_roster(&core.0, catalog.as_deref(), &mut roster);
                    benilla_protocol::messages::CHAR_DELETE_SUCCESS
                } else {
                    0x2F
                };
                action_results.write(CharActionResultMessage {
                    action: CharAction::Delete,
                    code,
                });
            }
            CharRequest::Create(request) => {
                let code = match create_local_character(&mut core.0, catalog.as_deref(), request) {
                    Ok(()) => {
                        save_after_command(&core.0, &mut persistence);
                        send_roster(&core.0, catalog.as_deref(), &mut roster);
                        benilla_protocol::messages::CHAR_CREATE_SUCCESS
                    }
                    Err(code) => code,
                };
                action_results.write(CharActionResultMessage {
                    action: CharAction::Create,
                    code,
                });
            }
            CharRequest::Abandon => {
                if core.0.active().is_some() {
                    if let Err(error) = core.0.leave_world() {
                        warn!("offline world: could not leave cleanly: {error}");
                    }
                    save_after_command(&core.0, &mut persistence);
                }
            }
        }
    }
}

fn create_local_character(
    core: &mut GameCore,
    catalog: Option<&crate::entities::CharCreate>,
    request: CharCreateReq,
) -> Result<(), u8> {
    let invalid = benilla_protocol::messages::CHAR_CREATE_ERROR;
    let Some(catalog) = catalog else {
        return Err(invalid);
    };
    if request.gender > 1 || !catalog.0.allows(request.race, request.class) {
        return Err(invalid);
    }
    let Some(ranges) = catalog.0.ranges(request.race, request.gender) else {
        return Err(invalid);
    };
    if request.skin >= ranges.skin
        || request.face >= ranges.face
        || request.hair_style >= ranges.hair_style
        || request.hair_color >= ranges.hair_color
        || request.facial_hair >= ranges.facial_hair
    {
        return Err(invalid);
    }

    let appearance = Appearance {
        race: request.race,
        class: request.class,
        gender: request.gender,
        skin: request.skin,
        face: request.face,
        hair_style: request.hair_style,
        hair_color: request.hair_color,
        facial_hair: request.facial_hair,
    };
    let Some(start) = starting_position(request.race) else {
        return Err(invalid);
    };
    let id = CharacterId::parse(format!(
        "{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    ))
    .expect("random 128-bit character ID is lowercase hexadecimal");
    core.create_character(id, &request.name, appearance, start)
        .map(|_| ())
        .map_err(|error| match error {
            benilla_game_core::CoreError::DuplicateName => {
                benilla_protocol::messages::CHAR_CREATE_NAME_IN_USE
            }
            benilla_game_core::CoreError::RosterFull => {
                benilla_protocol::messages::CHAR_CREATE_SERVER_LIMIT
            }
            _ => invalid,
        })
}

fn inventory_type_to_roster_slot(inventory_type: u8) -> Option<usize> {
    Some(match inventory_type {
        1 => 0,        // head
        2 => 1,        // neck
        3 => 2,        // shoulders
        4 => 3,        // shirt
        5 | 20 => 4,   // chest or robe
        6 => 5,        // waist
        7 => 6,        // legs
        8 => 7,        // feet
        9 => 8,        // wrists
        10 => 9,       // hands
        11 => 10,      // finger
        12 => 12,      // trinket
        13 | 17 => 15, // weapon or two-hand
        14 => 16,      // shield/off hand
        15 => 17,      // ranged
        16 => 14,      // cloak
        19 => 18,      // tabard
        _ => return None,
    })
}

fn emit_world_entry(
    channels: &OfflineChannels,
    snapshot: &benilla_game_core::CharacterSnapshot,
    hostiles: &[HostileState],
    catalog: Option<&crate::entities::CharCreate>,
    creatures: Option<&crate::entities::Creatures>,
    factions: Option<&crate::target::Factions>,
    names: &mut crate::names::NameCache,
) {
    let guid = snapshot.id.presentation_guid();
    let position = [
        snapshot.position.x,
        snapshot.position.y,
        snapshot.position.z,
    ];
    let appearance = snapshot.appearance;
    let race_class_gender = u32::from(appearance.race)
        | (u32::from(appearance.class) << 8)
        | (u32::from(appearance.gender) << 16);
    let player_bytes = u32::from(appearance.skin)
        | (u32::from(appearance.face) << 8)
        | (u32::from(appearance.hair_style) << 16)
        | (u32::from(appearance.hair_color) << 24);
    let display_id =
        catalog.and_then(|catalog| catalog.0.body_display(appearance.race, appearance.gender));
    let mut pairs = vec![
        (2, 0x19),                               // OBJECT_FIELD_TYPE: object, unit, player
        (22, 100),                                // UNIT_FIELD_HEALTH
        (28, 100),                                // UNIT_FIELD_MAXHEALTH
        (34, u32::from(snapshot.level)),          // UNIT_FIELD_LEVEL
        (36, race_class_gender),                  // UNIT_FIELD_BYTES_0
        (193, player_bytes),                      // PLAYER_BYTES
        (194, u32::from(appearance.facial_hair)), // PLAYER_BYTES_2
    ];
    if let Some(faction) = factions.and_then(|factions| factions.race_template(appearance.race)) {
        pairs.push((35, faction));
    }
    if appearance.class == 8 {
        pairs.extend([
            (23, benilla_game_core::STARTER_MANA),
            (29, benilla_game_core::STARTER_MANA),
        ]);
    }
    if let Some(display_id) = display_id {
        pairs.push((131, display_id));
        pairs.push((132, display_id));
    }
    let fields = benilla_protocol::ObjectFields::from_pairs(&pairs)
        .into_created(benilla_protocol::messages::ObjectType::Player);
    let mut events = vec![
        SessionEvent::Connected {
            self_guid: guid,
            name: snapshot.name.clone(),
            billing_time_rested: 0,
            tutorial_flags: None,
            addon_info: None,
        },
        SessionEvent::Worldport {
            map_id: snapshot.position.map_id,
            position,
            orientation: snapshot.position.orientation,
            needs_ack: false,
        },
        SessionEvent::ObjectCreate {
            guid,
            kind: benilla_protocol::EntityKind::Player,
            display_id,
            position,
            orientation: snapshot.position.orientation,
            scale: 1.0,
            speeds: None,
            mover: None,
            transport_progress: None,
            transport: None,
            spline: None,
            fields,
        },
    ];
    let (spell_ids, buttons) = starter_loadout(appearance.class);
    events.push(SessionEvent::SpellBook {
        spell_ids,
        cooldowns: Vec::new(),
    });
    events.push(SessionEvent::ActionButtons { buttons });
    for hostile in hostiles {
        let race = snapshot.appearance.race;
        let entry = hostile_entry(race);
        let guid = hostile_guid(race, hostile.spawn_id);
        let model_path = match race {
            10 => "Creature\\ManaWyrm\\ManaWyrm.m2",
            11 => "Creature\\Owlkin\\Owlkin.m2",
            _ => "Creature\\Wolf\\Wolf.m2",
        };
        let display_id = creatures.and_then(|catalog| catalog.display_for_model_path(model_path));
        if display_id.is_none() {
            warn!("offline practice hostile has no renderable build-8606 display for {model_path}");
        }
        let scale = display_id
            .and_then(|display_id| creatures.and_then(|catalog| catalog.model_scale(display_id)))
            .unwrap_or(1.0);
        let mut mob_fields = vec![
            (2, 0x9), // OBJECT_FIELD_TYPE: object and unit
            (3, entry),
            (4, scale.to_bits()), // OBJECT_FIELD_SCALE_X: display/model scale folded by the adapter
            (22, hostile.health),
            (28, hostile.max_health),
            (34, 1),
            (35, 14), // Monster faction template
            (130, 1.5f32.to_bits()),
        ];
        if let Some(display_id) = display_id {
            mob_fields.extend([(131, display_id), (132, display_id)]);
        }
        names.insert_creature(
            entry,
            Some(crate::names::CreatureRecord {
                name: hostile_name(race).to_string(),
                subname: None,
                creature_type: 1,
                pet_family: 0,
                rank: 0,
                type_flags: 0,
                civilian: false,
                racial_leader: false,
                display_id: display_id.unwrap_or(0),
            }),
        );
        events.push(SessionEvent::ObjectCreate {
            guid,
            kind: EntityKind::Unit,
            display_id,
            position: [hostile.position.x, hostile.position.y, hostile.position.z],
            orientation: hostile.position.orientation,
            scale,
            speeds: None,
            mover: None,
            transport_progress: None,
            transport: None,
            spline: None,
            fields: ObjectFields::from_pairs(&mob_fields)
                .into_created(benilla_protocol::messages::ObjectType::Unit),
        });
    }
    events.push(SessionEvent::TimeSpeed {
        hours: ((snapshot.game_time_minutes % 1440) / 60) as u8,
        minutes: (snapshot.game_time_minutes % 60) as u8,
        day_serial: (snapshot.game_time_minutes / 1440) as u32,
        timescale: 1.0,
    });
    for event in events {
        if channels.events.send(event).is_err() {
            error!("offline session adapter: event receiver was dropped");
            break;
        }
    }
}

fn starter_loadout(class: u8) -> (Vec<u32>, Vec<benilla_protocol::messages::ActionButton>) {
    let mut spell_ids = vec![benilla_game_core::ATTACK_SPELL_ID];
    let mut buttons = vec![benilla_protocol::messages::ActionButton {
        slot: 0,
        action: benilla_game_core::ATTACK_SPELL_ID,
        kind: benilla_protocol::messages::ACTION_KIND_SPELL,
    }];
    if class == 8 {
        spell_ids.push(benilla_game_core::FIREBALL_SPELL_ID);
        buttons.push(benilla_protocol::messages::ActionButton {
            slot: 1,
            action: benilla_game_core::FIREBALL_SPELL_ID,
            kind: benilla_protocol::messages::ACTION_KIND_SPELL,
        });
    }
    (spell_ids, buttons)
}

fn starting_position(race: u8) -> Option<WorldPosition> {
    // Project-authored build-8606 starting anchors; gameplay rules and provenance are recorded in
    // `crates/benilla-game-core/RULES.md`.
    let (map_id, x, y, z) = match race {
        1 => (0, -8949.95, -132.49, 83.53),
        2 | 8 => (1, -618.518, -4251.67, 38.718),
        3 | 7 => (0, -6240.32, 331.033, 382.758),
        4 => (1, 10311.3, 832.463, 1326.41),
        5 => (0, 1676.35, 1678.21, 121.67),
        6 => (1, -2917.58, -257.98, 52.9968),
        10 => (530, 10349.6, -6357.3, 33.4),
        11 => (530, -3961.64, -13931.2, 100.615),
        _ => return None,
    };
    Some(WorldPosition {
        map_id,
        x,
        y,
        z,
        orientation: 0.0,
    })
}

fn hostile_entry(race: u8) -> u32 {
    match race {
        10 => 90_002,
        11 => 90_003,
        _ => 90_001,
    }
}

fn hostile_name(race: u8) -> &'static str {
    match race {
        10 => "Sunstrider Mana Wyrm",
        11 => "Azuremyst Owlkin",
        _ => "Training Wolf",
    }
}

fn hostile_guid(race: u8, spawn_id: u32) -> u64 {
    (u64::from(benilla_protocol::guid::HIGH_UNIT) << 48)
        | (u64::from(hostile_entry(race)) << 24)
        | u64::from(spawn_id)
}

fn drive_offline_gameplay(
    channels: Res<OfflineChannels>,
    mut core: ResMut<OfflineGameCore>,
    mut persistence: ResMut<OfflinePersistence>,
    time: Res<Time>,
    spells: Option<Res<crate::ui_action::Spells>>,
) {
    while let Ok(command) = channels.commands.try_recv() {
        match &command {
            crate::net::ClientCommand::CancelCast { spell_id } => {
                core.0.cancel_fireball(*spell_id);
                continue;
            }
            crate::net::ClientCommand::AttackSwing { guid } => {
                start_offline_auto_attack(&mut core.0, &channels, *guid);
                continue;
            }
            crate::net::ClientCommand::AttackStop => {
                stop_offline_auto_attack(&mut core.0, &channels);
                continue;
            }
            _ => {}
        }
        if matches!(
            &command,
            crate::net::ClientCommand::Logout | crate::net::ClientCommand::ForceLogout
        ) {
            if core.0.active().is_some() {
                match core.0.leave_world() {
                    Ok(_) => {
                        save_after_command(&core.0, &mut persistence);
                        send_gameplay_events(&channels, [SessionEvent::LoggedOut]);
                    }
                    Err(error) => warn!("offline gameplay: logout failed: {error}"),
                }
            }
            continue;
        }
        let mana_cost = match &command {
            crate::net::ClientCommand::CastSpell { spell_id, .. } => {
                spells
                    .as_deref()
                    .and_then(|spells| {
                        let spell = spells.catalog.get(*spell_id)?;
                        let cast_time_ms = spells.cast_time_ms(
                            spell,
                            1,
                            &crate::spell::SpellModifiers::default(),
                        );
                        Some((spell.mana_cost, cast_time_ms))
                    })
                    .or_else(|| {
                        (*spell_id == benilla_game_core::FIREBALL_SPELL_ID).then_some((
                            benilla_game_core::FIREBALL_MANA_COST,
                            benilla_game_core::FIREBALL_CAST_MS,
                        ))
                    })
            }
            _ => None,
        };
        if let Some((mana_cost, cast_time_ms)) = mana_cost {
            start_offline_cast(&mut core.0, &channels, command, mana_cost, cast_time_ms);
        }
    }
    if core.0.active().is_none() {
        return;
    }
    let elapsed_ms = (time.delta_secs_f64() * 1000.0).round() as u32;
    advance_offline_combat(&mut core.0, &channels, elapsed_ms);
}

fn start_offline_cast(
    core: &mut GameCore,
    channels: &OfflineChannels,
    command: crate::net::ClientCommand,
    mana_cost: u32,
    cast_time_ms: u32,
) {
    let crate::net::ClientCommand::CastSpell { spell_id, target } = command else {
        return;
    };
    let Some(id) = core.active().cloned() else {
        return;
    };
    let Some(snapshot) = core.snapshot(&id) else {
        return;
    };
    let Some(spawn_id) = core
        .hostiles()
        .iter()
        .find(|hostile| target == Some(hostile_guid(snapshot.appearance.race, hostile.spawn_id)))
        .map(|hostile| hostile.spawn_id)
    else {
        return;
    };
    match core.begin_fireball(spell_id, spawn_id, mana_cost, cast_time_ms) {
        Ok(()) => {
            let player_guid = id.presentation_guid();
            send_gameplay_events(
                channels,
                [
                    SessionEvent::SpellStart {
                        caster: player_guid,
                        spell_id,
                        cast_flags: 0x2,
                        cast_time_ms,
                        target,
                        ammo_display_id: None,
                    },
                    SessionEvent::ObjectValues {
                        guid: player_guid,
                        fields: ObjectFields::from_pairs(&[(23, core.active_mana())]),
                    },
                ],
            );
        }
        Err(error) => debug!("offline gameplay: rejected spell {spell_id}: {error}"),
    }
}

fn start_offline_auto_attack(core: &mut GameCore, channels: &OfflineChannels, target: u64) {
    let Some(id) = core.active().cloned() else {
        return;
    };
    let Some(snapshot) = core.snapshot(&id) else {
        return;
    };
    let Some(spawn_id) = core
        .hostiles()
        .iter()
        .find(|hostile| target == hostile_guid(snapshot.appearance.race, hostile.spawn_id))
        .map(|hostile| hostile.spawn_id)
    else {
        return;
    };
    match core.start_auto_attack(spawn_id) {
        Ok(()) => send_gameplay_events(
            channels,
            [SessionEvent::AttackStart {
                attacker: id.presentation_guid(),
                victim: target,
            }],
        ),
        Err(error) => debug!("offline gameplay: rejected auto-attack at {target:#x}: {error}"),
    }
}

fn stop_offline_auto_attack(core: &mut GameCore, channels: &OfflineChannels) {
    let Some(spawn_id) = core.stop_auto_attack() else {
        return;
    };
    let Some(id) = core.active() else { return };
    let Some(snapshot) = core.snapshot(id) else {
        return;
    };
    send_gameplay_events(
        channels,
        [SessionEvent::AttackStop {
            attacker: id.presentation_guid(),
            victim: hostile_guid(snapshot.appearance.race, spawn_id),
        }],
    );
}

fn advance_offline_combat(core: &mut GameCore, channels: &OfflineChannels, elapsed_ms: u32) {
    if core.active().is_none() {
        return;
    }
    let updates = core.advance_combat(elapsed_ms);
    for update in updates {
        match update {
            CombatUpdate::Impact(impact) => {
                let Some(id) = core.active() else {
                    continue;
                };
                let Some(snapshot) = core.snapshot(id) else {
                    continue;
                };
                let guid = hostile_guid(snapshot.appearance.race, impact.spawn_id);
                let events = [
                    SessionEvent::SpellGo {
                        caster: id.presentation_guid(),
                        spell_id: benilla_game_core::FIREBALL_SPELL_ID,
                        cast_flags: 0x100,
                        hits: vec![guid],
                        misses: Vec::new(),
                        target: Some(guid),
                        go_target: None,
                        dest: None,
                        ammo_display_id: None,
                        item_caster: None,
                    },
                    SessionEvent::SpellDamageLog(benilla_protocol::messages::SpellDamageLog {
                        target: guid,
                        attacker: id.presentation_guid(),
                        spell_id: benilla_game_core::FIREBALL_SPELL_ID,
                        damage: impact.damage,
                        school: 4,
                        absorb: 0,
                        resist: 0,
                        periodic: false,
                        blocked: 0,
                        hit_info: 0,
                    }),
                    SessionEvent::ObjectValues {
                        guid,
                        fields: ObjectFields::from_pairs(&[
                            (22, impact.remaining_health),
                            (143, if impact.killed { 0x20 } else { 0 }),
                        ]),
                    },
                ];
                send_gameplay_events(channels, events);
            }
            CombatUpdate::MeleeImpact(impact) => {
                let Some(id) = core.active() else {
                    continue;
                };
                let Some(snapshot) = core.snapshot(id) else {
                    continue;
                };
                let victim = hostile_guid(snapshot.appearance.race, impact.spawn_id);
                send_gameplay_events(
                    channels,
                    [
                        SessionEvent::AttackerState(benilla_protocol::messages::AttackerState {
                            attacker: id.presentation_guid(),
                            victim,
                            hit_info: 0,
                            damage: impact.damage,
                            victim_state: 1,
                            absorb: 0,
                            resist: 0,
                            blocked: 0,
                            melee_spell_id: 0,
                            school: 0,
                        }),
                        SessionEvent::ObjectValues {
                            guid: victim,
                            fields: ObjectFields::from_pairs(&[
                                (22, impact.remaining_health),
                                (143, if impact.killed { 0x20 } else { 0 }),
                            ]),
                        },
                    ],
                );
            }
            CombatUpdate::Respawned { spawn_id } => {
                let Some(id) = core.active() else {
                    continue;
                };
                let Some(snapshot) = core.snapshot(id) else {
                    continue;
                };
                if let Some(hostile) = core.hostile(spawn_id) {
                    let event = SessionEvent::ObjectValues {
                        guid: hostile_guid(snapshot.appearance.race, spawn_id),
                        fields: ObjectFields::from_pairs(&[(22, hostile.health), (143, 0)]),
                    };
                    send_gameplay_events(channels, [event]);
                }
            }
        }
    }
}

fn send_gameplay_events(
    channels: &OfflineChannels,
    events: impl IntoIterator<Item = SessionEvent>,
) {
    for event in events {
        if channels.events.send(event).is_err() {
            error!("offline gameplay adapter: event receiver was dropped");
            break;
        }
    }
}

fn save_after_command(core: &GameCore, persistence: &mut OfflinePersistence) {
    if !persistence.writable {
        return;
    }
    if let Err(error) = save_core(core) {
        persistence.writable = false;
        error!("offline save failed; persistence disabled: {error}");
    }
}

fn update_local_player(
    time: Res<Time>,
    player: Res<crate::player::Player>,
    mut core: ResMut<OfflineGameCore>,
    mut persistence: ResMut<OfflinePersistence>,
    mut save_elapsed: Local<f32>,
    mut exits: MessageReader<AppExit>,
) {
    let exiting = exits.read().next().is_some();
    let Some(id) = core.0.active().cloned() else {
        return;
    };
    if player.active {
        let [x, y, z] = benilla_assets::coords::bevy_to_wow(player.pos);
        let Some(snapshot) = core.0.snapshot(&id) else {
            return;
        };
        let position = benilla_game_core::WorldPosition {
            map_id: snapshot.position.map_id,
            x,
            y,
            z,
            orientation: player.facing(),
        };
        let elapsed_ms = (time.delta_secs_f64() * 1000.0).round() as u64;
        if let Err(error) = core.0.update_active(position, elapsed_ms) {
            warn!("offline simulation rejected player state: {error}");
            return;
        }
        *save_elapsed += time.delta_secs();
    }
    if exiting || *save_elapsed >= 5.0 {
        *save_elapsed = 0.0;
        save_after_command(&core.0, &mut persistence);
    }
}

fn exit_after_roster_boot(
    state: Res<State<crate::char_select::ClientState>>,
    mut frames: Local<u8>,
    mut exit: MessageWriter<AppExit>,
) {
    if *state.get() != crate::char_select::ClientState::CharSelect {
        return;
    }
    *frames = frames.saturating_add(1);
    if *frames == 3 {
        info!("offline: local roster reached without transport");
        exit.write(AppExit::Success);
    }
}

fn enter_offline_smoke_character(
    state: Res<State<crate::char_select::ClientState>>,
    core: Res<OfflineGameCore>,
    pick: Res<crate::net::CharPick>,
    mut requested: Local<bool>,
) {
    if *requested || *state.get() != crate::char_select::ClientState::CharSelect {
        return;
    }
    let Some(id) = core.0.selected() else {
        return;
    };
    if pick
        .0
        .send(CharRequest::Enter(id.presentation_guid()))
        .is_ok()
    {
        *requested = true;
        info!("offline smoke: entering selected local character");
    }
}

fn exit_after_world_smoke(
    mut commands: Commands,
    state: Res<State<crate::char_select::ClientState>>,
    loading: Res<crate::loading_screen::LoadingScreen>,
    core: Res<OfflineGameCore>,
    player: Res<crate::player::Player>,
    camera: Query<&Transform, With<benilla_world::view::WorldCamera>>,
    self_store: Query<&crate::net::ObjectStore, With<crate::net::SelfPlayer>>,
    time: Res<Time>,
    mut entered_at: Local<Option<f32>>,
    mut shot_requested: Local<bool>,
    mut smoke_seconds: Local<Option<f32>>,
    mut exit: MessageWriter<AppExit>,
) {
    if *state.get() != crate::char_select::ClientState::InWorld || loading.covering() {
        *entered_at = None;
        return;
    }
    let started = *entered_at.get_or_insert(time.elapsed_secs());
    let elapsed = time.elapsed_secs() - started;
    let hold = *smoke_seconds.get_or_insert_with(|| {
        std::env::var("BENILLA_OFFLINE_SMOKE_SECONDS")
            .ok()
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .unwrap_or(6.0)
    });
    if elapsed >= hold && !*shot_requested {
        let camera_pose = camera
            .single()
            .ok()
            .map(|transform| (transform.translation, transform.forward().as_vec3()));
        let self_health = self_store
            .single()
            .ok()
            .map(|store| (store.0.unit_health(), store.0.unit_max_health()));
        info!(
            "offline smoke: player active={} settling={} pos={:?}, camera={camera_pose:?}, health={self_health:?}",
            player.active, player.settling, player.pos,
        );
        if std::env::var_os("BENILLA_OFFLINE_SMOKE_CAST").is_some() {
            let first_health = core
                .0
                .hostile(benilla_game_core::HOSTILE_SPAWN_ID)
                .map(|mob| mob.health);
            info!(
                "offline smoke: combat spawned={} first_hostile_health={first_health:?} mana={}",
                core.0.hostiles().len(),
                core.0.active_mana(),
            );
        }
        if let Ok(path) = std::env::var("BENILLA_OFFLINE_SMOKE_SHOT") {
            use bevy::render::view::screenshot::{Screenshot, save_to_disk};
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(path.clone()));
            info!("offline smoke: capturing world to {path}");
        }
        *shot_requested = true;
    }
    if elapsed >= hold + 1.0 {
        info!("offline smoke: world stayed active for {hold:.0} seconds");
        exit.write(AppExit::Success);
    }
}

/// `BENILLA_OFFLINE_SMOKE_CAST=1` sends two real offline Fireball commands at the first spawned
/// hostile, through the same command channel as the stock action bar.
fn drive_offline_smoke_cast(
    state: Res<State<crate::char_select::ClientState>>,
    loading: Res<crate::loading_screen::LoadingScreen>,
    time: Res<Time>,
    core: Res<OfflineGameCore>,
    spells: Option<Res<crate::ui_action::Spells>>,
    commands: Res<crate::net::NetCommands>,
    mut casts_sent: Local<u8>,
    mut entered_at: Local<Option<f32>>,
) {
    if std::env::var_os("BENILLA_OFFLINE_SMOKE_CAST").is_none() {
        return;
    }
    if *state.get() != crate::char_select::ClientState::InWorld || loading.covering() {
        *entered_at = None;
        *casts_sent = 0;
        return;
    }
    let Some(spells) = spells else {
        return;
    };
    if spells
        .catalog
        .get(benilla_game_core::FIREBALL_SPELL_ID)
        .is_none()
    {
        return;
    }
    let elapsed = time.elapsed_secs() - *entered_at.get_or_insert(time.elapsed_secs());
    if *casts_sent >= 2 || elapsed < f32::from(*casts_sent) * 2.0 {
        return;
    }
    let Some(id) = core.0.active() else {
        return;
    };
    let Some(snapshot) = core.0.snapshot(id) else {
        return;
    };
    let target = hostile_guid(
        snapshot.appearance.race,
        benilla_game_core::HOSTILE_SPAWN_ID,
    );
    if commands
        .0
        .send(crate::net::ClientCommand::CastSpell {
            spell_id: benilla_game_core::FIREBALL_SPELL_ID,
            target: Some(target),
        })
        .is_ok()
    {
        *casts_sent += 1;
        info!("offline smoke: sent Fireball command {}/2", *casts_sent);
    }
}

/// `BENILLA_OFFLINE_SMOKE_WALK_SECONDS`: optionally exercise the real forward binding after the
/// loading cover clears; the default smoke remains stationary.
fn drive_offline_smoke_walk(
    state: Res<State<crate::char_select::ClientState>>,
    loading: Res<crate::loading_screen::LoadingScreen>,
    time: Res<Time>,
    mut keys: ResMut<ButtonInput<KeyCode>>,
    mut keyboard: MessageWriter<bevy::input::keyboard::KeyboardInput>,
    mut walk_seconds: Local<Option<f32>>,
    mut started: Local<Option<f32>>,
    mut finished: Local<bool>,
) {
    let seconds = *walk_seconds.get_or_insert_with(|| {
        std::env::var("BENILLA_OFFLINE_SMOKE_WALK_SECONDS")
            .ok()
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .unwrap_or(0.0)
    });
    if seconds == 0.0 {
        return;
    }
    if *state.get() != crate::char_select::ClientState::InWorld || loading.covering() {
        *started = None;
        keys.release(KeyCode::KeyW);
        return;
    }
    let start = *started.get_or_insert_with(|| time.elapsed_secs());
    if time.elapsed_secs() - start < seconds {
        keys.press(KeyCode::KeyW);
        keyboard.write(bevy::input::keyboard::KeyboardInput {
            key_code: KeyCode::KeyW,
            logical_key: bevy::input::keyboard::Key::Unidentified(
                bevy::input::keyboard::NativeKey::Unidentified,
            ),
            state: bevy::input::ButtonState::Pressed,
            text: None,
            repeat: time.elapsed_secs() > start,
            window: Entity::PLACEHOLDER,
        });
    } else if !*finished {
        keys.release(KeyCode::KeyW);
        keyboard.write(bevy::input::keyboard::KeyboardInput {
            key_code: KeyCode::KeyW,
            logical_key: bevy::input::keyboard::Key::Unidentified(
                bevy::input::keyboard::NativeKey::Unidentified,
            ),
            state: bevy::input::ButtonState::Released,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        });
        info!("offline smoke: forward input held for {seconds:.1}s");
        *finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    #[test]
    fn game_core_stays_engine_free_and_is_available_to_the_adapter() {
        let core = OfflineGameCore::default();
        assert_eq!(core.0, benilla_game_core::GameCore::default());
    }

    #[test]
    fn new_characters_start_in_their_race_area_including_humans_in_northshire() {
        let human = starting_position(1).unwrap();
        assert_eq!(human.map_id, 0);
        assert_eq!((human.x, human.y, human.z), (-8949.95, -132.49, 83.53));
        assert_eq!(starting_position(10).unwrap().map_id, 530);
        assert_eq!(starting_position(11).unwrap().map_id, 530);
        for race in [2, 3, 4, 5, 6, 7, 8] {
            let start = starting_position(race).unwrap();
            assert!(start.x.is_finite() && start.y.is_finite() && start.z.is_finite());
        }
        assert!(starting_position(9).is_none());
    }

    #[test]
    fn starter_loadouts_always_include_attack_and_only_mages_get_fireball() {
        let (mage_spells, mage_buttons) = starter_loadout(8);
        assert_eq!(
            mage_spells,
            [
                benilla_game_core::ATTACK_SPELL_ID,
                benilla_game_core::FIREBALL_SPELL_ID
            ]
        );
        assert_eq!(
            mage_buttons
                .iter()
                .map(|button| (button.slot, button.action))
                .collect::<Vec<_>>(),
            [
                (0, benilla_game_core::ATTACK_SPELL_ID),
                (1, benilla_game_core::FIREBALL_SPELL_ID)
            ]
        );

        let (warrior_spells, warrior_buttons) = starter_loadout(1);
        assert_eq!(warrior_spells, [benilla_game_core::ATTACK_SPELL_ID]);
        assert_eq!(warrior_buttons.len(), 1);
        assert_eq!(warrior_buttons[0].action, benilla_game_core::ATTACK_SPELL_ID);
    }

    #[test]
    fn offline_action_bar_cast_damages_and_respawns_a_spawned_hostile() {
        let appearance = Appearance {
            race: 1,
            class: 8,
            gender: 0,
            skin: 0,
            face: 0,
            hair_style: 0,
            hair_color: 0,
            facial_hair: 0,
        };
        let id = CharacterId::parse("00000000000000000000000000000001").unwrap();
        let mut core = GameCore::default();
        core.create_character(
            id.clone(),
            "Alaera",
            appearance,
            starting_position(1).unwrap(),
        )
        .unwrap();
        core.enter_world().unwrap();

        let (_pick_tx, picks) = crossbeam_channel::unbounded();
        let (events, received) = crossbeam_channel::unbounded();
        let (_command_tx, commands) = crossbeam_channel::unbounded();
        let channels = OfflineChannels {
            picks,
            events,
            commands,
        };
        let mut names = crate::names::NameCache::default();
        emit_world_entry(
            &channels,
            core.snapshot(&id).unwrap(),
            core.hostiles(),
            None,
            None,
            None,
            &mut names,
        );
        let entry_events: Vec<_> = received.try_iter().collect();
        assert!(entry_events.iter().any(|event| matches!(
            event,
            SessionEvent::SpellBook { spell_ids, .. }
                if spell_ids == &[
                    benilla_game_core::ATTACK_SPELL_ID,
                    benilla_game_core::FIREBALL_SPELL_ID
                ]
        )));
        assert!(entry_events.iter().any(|event| matches!(
            event,
            SessionEvent::ActionButtons { buttons }
                if buttons.iter().any(|button| button.slot == 0
                    && button.action == benilla_game_core::ATTACK_SPELL_ID)
                    && buttons.iter().any(|button| button.slot == 1
                        && button.action == benilla_game_core::FIREBALL_SPELL_ID)
        )));
        let player_guid = id.presentation_guid();
        let player = entry_events.iter().find_map(|event| match event {
            SessionEvent::ObjectCreate {
                guid,
                kind: EntityKind::Player,
                fields,
                ..
            } if *guid == player_guid => Some(fields),
            _ => None,
        });
        assert_eq!(
            player.unwrap().object_type(),
            Some(benilla_protocol::messages::ObjectType::Player)
        );
        assert_eq!(
            entry_events
                .iter()
                .filter(|event| matches!(
                    event,
                    SessionEvent::ObjectCreate {
                        kind: EntityKind::Unit,
                        ..
                    }
                ))
                .count(),
            3
        );
        let hostile_guid = hostile_guid(appearance.race, benilla_game_core::HOSTILE_SPAWN_ID);
        let mob = entry_events.iter().find_map(|event| match event {
            SessionEvent::ObjectCreate {
                guid,
                kind: EntityKind::Unit,
                fields,
                ..
            } if *guid == hostile_guid => Some(fields),
            _ => None,
        });
        assert_eq!(mob.unwrap().unit_health(), Some(60));
        assert_eq!(
            mob.unwrap().object_type(),
            Some(benilla_protocol::messages::ObjectType::Unit)
        );

        for expected_health in [30, 0] {
            start_offline_cast(
                &mut core,
                &channels,
                crate::net::ClientCommand::CastSpell {
                    spell_id: benilla_game_core::FIREBALL_SPELL_ID,
                    target: Some(hostile_guid),
                },
                30,
                benilla_game_core::FIREBALL_CAST_MS,
            );
            assert!(matches!(
                received.try_recv().unwrap(),
                SessionEvent::SpellStart { spell_id: 133, .. }
            ));
            let mana = received.try_recv().unwrap();
            assert!(matches!(
                mana,
                SessionEvent::ObjectValues { guid, fields }
                    if guid == id.presentation_guid()
                        && fields.unit_power(0) == Some(if expected_health == 30 { 70 } else { 40 })
            ));
            advance_offline_combat(&mut core, &channels, benilla_game_core::FIREBALL_CAST_MS);
            let impact_events: Vec<_> = received.try_iter().collect();
            assert!(
                impact_events
                    .iter()
                    .any(|event| matches!(event, SessionEvent::SpellGo { spell_id: 133, .. }))
            );
            assert!(impact_events.iter().any(
                |event| matches!(event, SessionEvent::SpellDamageLog(log) if log.damage == 30)
            ));
            assert!(impact_events.iter().any(|event| matches!(
                event,
                SessionEvent::ObjectValues { guid, fields }
                    if *guid == hostile_guid && fields.unit_health() == Some(expected_health)
            )));
            if expected_health == 0 {
                assert!(impact_events.iter().any(|event| matches!(
                    event,
                    SessionEvent::ObjectValues { guid, fields }
                        if *guid == hostile_guid && fields.unit_dynamic_flags() == 0x20
                )));
            }
        }
        assert_eq!(
            core.hostile(benilla_game_core::HOSTILE_SPAWN_ID)
                .unwrap()
                .health,
            0
        );
        advance_offline_combat(&mut core, &channels, benilla_game_core::HOSTILE_RESPAWN_MS);
        assert_eq!(
            core.hostile(benilla_game_core::HOSTILE_SPAWN_ID)
                .unwrap()
                .health,
            60
        );
        assert!(matches!(
            received.try_recv().unwrap(),
            SessionEvent::ObjectValues { guid, ref fields }
                if guid == hostile_guid && fields.unit_health() == Some(60)
        ));
    }

    #[test]
    fn offline_attack_action_starts_swings_stops_and_updates_hostile_health() {
        let id = CharacterId::parse("00000000000000000000000000000001").unwrap();
        let appearance = Appearance {
            race: 1,
            class: 8,
            gender: 0,
            skin: 0,
            face: 0,
            hair_style: 0,
            hair_color: 0,
            facial_hair: 0,
        };
        let mut core = GameCore::default();
        core.create_character(id.clone(), "Alaera", appearance, starting_position(1).unwrap())
            .unwrap();
        core.enter_world().unwrap();
        let start = core.snapshot(&id).unwrap().position;
        core.update_active(
            WorldPosition {
                x: start.x + 8.0,
                ..start
            },
            0,
        )
        .unwrap();

        let (_pick_tx, picks) = crossbeam_channel::unbounded();
        let (events, received) = crossbeam_channel::unbounded();
        let (commands, command_receiver) = crossbeam_channel::unbounded();
        let mut app = App::new();
        app.insert_resource(OfflineChannels {
            picks,
            events,
            commands: command_receiver,
        })
        .insert_resource(OfflineGameCore(core))
        .insert_resource(OfflinePersistence { writable: false })
        .insert_resource(Time::<()>::default());

        let target = hostile_guid(appearance.race, benilla_game_core::HOSTILE_SPAWN_ID);
        commands
            .send(crate::net::ClientCommand::AttackSwing { guid: target })
            .unwrap();
        app.world_mut()
            .run_system_once(drive_offline_gameplay)
            .expect("offline attack command is handled");
        assert!(matches!(
            received.try_recv().unwrap(),
            SessionEvent::AttackStart { attacker, victim }
                if attacker == id.presentation_guid() && victim == target
        ));

        let channels = app.world().resource::<OfflineChannels>().clone();
        advance_offline_combat(
            &mut app.world_mut().resource_mut::<OfflineGameCore>().0,
            &channels,
            benilla_game_core::MELEE_SWING_MS,
        );
        let swing: Vec<_> = received.try_iter().collect();
        assert!(swing.iter().any(|event| matches!(
            event,
            SessionEvent::AttackerState(state)
                if state.attacker == id.presentation_guid()
                    && state.victim == target
                    && state.damage == benilla_game_core::MELEE_DAMAGE
        )));
        assert!(swing.iter().any(|event| matches!(
            event,
            SessionEvent::ObjectValues { guid, fields }
                if *guid == target
                    && fields.unit_health()
                        == Some(benilla_game_core::HOSTILE_MAX_HEALTH - benilla_game_core::MELEE_DAMAGE)
        )));

        commands.send(crate::net::ClientCommand::AttackStop).unwrap();
        app.world_mut()
            .run_system_once(drive_offline_gameplay)
            .expect("offline stop command is handled");
        assert!(matches!(
            received.try_recv().unwrap(),
            SessionEvent::AttackStop { attacker, victim }
                if attacker == id.presentation_guid() && victim == target
        ));
        let channels = app.world().resource::<OfflineChannels>().clone();
        advance_offline_combat(
            &mut app.world_mut().resource_mut::<OfflineGameCore>().0,
            &channels,
            benilla_game_core::MELEE_SWING_MS,
        );
        assert!(
            received.try_iter().next().is_none(),
            "stopped attacks do no more damage"
        );
    }

    #[test]
    fn offline_fireball_uses_project_rules_when_spell_catalog_is_unavailable() {
        let id = CharacterId::parse("00000000000000000000000000000001").unwrap();
        let appearance = Appearance {
            race: 1,
            class: 8,
            gender: 0,
            skin: 0,
            face: 0,
            hair_style: 0,
            hair_color: 0,
            facial_hair: 0,
        };
        let mut core = GameCore::default();
        core.create_character(id.clone(), "Alaera", appearance, starting_position(1).unwrap())
            .unwrap();
        core.enter_world().unwrap();

        let (_pick_tx, picks) = crossbeam_channel::unbounded();
        let (events, received) = crossbeam_channel::unbounded();
        let (commands, command_receiver) = crossbeam_channel::unbounded();
        let mut app = App::new();
        app.insert_resource(OfflineChannels {
            picks,
            events,
            commands: command_receiver,
        })
        .insert_resource(OfflineGameCore(core))
        .insert_resource(OfflinePersistence { writable: false })
        .insert_resource(Time::<()>::default());

        let target = hostile_guid(appearance.race, benilla_game_core::HOSTILE_SPAWN_ID);
        commands
            .send(crate::net::ClientCommand::CastSpell {
                spell_id: benilla_game_core::FIREBALL_SPELL_ID,
                target: Some(target),
            })
            .unwrap();
        app.world_mut()
            .run_system_once(drive_offline_gameplay)
            .expect("offline Fireball cast is handled without a Spell.dbc resource");
        assert!(matches!(
            received.try_recv().unwrap(),
            SessionEvent::SpellStart {
                spell_id: benilla_game_core::FIREBALL_SPELL_ID,
                cast_time_ms: benilla_game_core::FIREBALL_CAST_MS,
                ..
            }
        ));
        assert!(matches!(
            received.try_recv().unwrap(),
            SessionEvent::ObjectValues { fields, .. }
                if fields.unit_power(0)
                    == Some(benilla_game_core::STARTER_MANA - benilla_game_core::FIREBALL_MANA_COST)
        ));

        let channels = app.world().resource::<OfflineChannels>().clone();
        advance_offline_combat(
            &mut app.world_mut().resource_mut::<OfflineGameCore>().0,
            &channels,
            benilla_game_core::FIREBALL_CAST_MS,
        );
        assert!(matches!(
            received.try_recv().unwrap(),
            SessionEvent::SpellGo { spell_id, .. }
                if spell_id == benilla_game_core::FIREBALL_SPELL_ID
        ));
        assert!(matches!(
            received.try_recv().unwrap(),
            SessionEvent::SpellDamageLog(log)
                if log.spell_id == benilla_game_core::FIREBALL_SPELL_ID && log.damage == 30
        ));
    }

    #[test]
    fn offline_logout_leaves_the_world_and_emits_the_normal_logout_event() {
        let id = CharacterId::parse("00000000000000000000000000000001").unwrap();
        let mut core = GameCore::default();
        core.create_character(
            id.clone(),
            "Alaera",
            Appearance {
                race: 1,
                class: 8,
                gender: 0,
                skin: 0,
                face: 0,
                hair_style: 0,
                hair_color: 0,
                facial_hair: 0,
            },
            starting_position(1).unwrap(),
        )
        .unwrap();
        core.enter_world().unwrap();
        let saved_position = WorldPosition {
            map_id: 0,
            x: -8900.0,
            y: -100.0,
            z: 84.0,
            orientation: 1.0,
        };
        core.update_active(saved_position, 0).unwrap();

        let (pick_tx, picks) = crossbeam_channel::unbounded();
        let (events, received) = crossbeam_channel::unbounded();
        let (commands, command_rx) = crossbeam_channel::unbounded();
        let mut app = App::new();
        app.insert_resource(OfflineChannels {
            picks,
            events,
            commands: command_rx,
        })
        .insert_resource(OfflineGameCore(core))
        .insert_resource(OfflinePersistence { writable: false })
        .insert_resource(Time::<()>::default());
        drop(pick_tx);

        commands
            .send(crate::net::ClientCommand::Logout)
            .expect("offline command receiver is alive");
        app.world_mut()
            .run_system_once(drive_offline_gameplay)
            .expect("offline gameplay command loop runs");

        let core = &app.world().resource::<OfflineGameCore>().0;
        assert!(
            core.active().is_none(),
            "logout clears the active character"
        );
        assert!(
            core.hostiles().is_empty(),
            "leaving clears the practice mobs"
        );
        assert_eq!(core.snapshot(&id).unwrap().position, saved_position);
        assert!(matches!(received.try_recv(), Ok(SessionEvent::LoggedOut)));
    }

    #[test]
    fn a_corrupt_primary_document_recovers_and_repairs_from_its_valid_backup() {
        let dir = std::env::temp_dir().join(format!("benilla-offline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("offline/profile.toml");
        local_state::write_atomic_bytes_with_backup(&path, b"42").unwrap();
        local_state::write_atomic_bytes_with_backup(&path, b"not a number").unwrap();

        let restored = read_checked_document(&path, |text| {
            text.parse::<u32>()
                .map_err(|error| SaveError::InvalidToml(error.to_string()))
        })
        .unwrap();

        assert_eq!(restored, Some(42));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "42");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn build_8606_blood_elf_creation_enters_map_530_with_local_appearance() {
        let Some(data) = benilla_formats::wow_data() else {
            eprintln!("skipping: no client install");
            return;
        };
        if !benilla_formats::is_tbc_data_dir(&data) {
            eprintln!("skipping: resolved install is not TBC");
            return;
        }
        let mut chain = benilla_formats::open_chain(&data).expect("open TBC archive chain");
        let catalog = crate::entities::CharCreate(
            benilla_formats::CharCreateCatalog::load(&mut chain)
                .expect("load TBC character creation catalog"),
        );
        let mut core = GameCore::default();
        create_local_character(
            &mut core,
            Some(&catalog),
            CharCreateReq {
                name: "Alaera".into(),
                race: 10,
                class: 8,
                gender: 0,
                skin: 0,
                face: 0,
                hair_style: 0,
                hair_color: 0,
                facial_hair: 0,
            },
        )
        .expect("create Blood Elf Mage");

        let id = core.selected().expect("created character selected").clone();
        let snapshot = core.snapshot(&id).expect("created snapshot");
        assert_eq!(snapshot.name, "Alaera");
        assert_eq!(
            (snapshot.appearance.race, snapshot.appearance.class),
            (10, 8)
        );
        assert_eq!(snapshot.position.map_id, 530);
        assert_eq!(core.enter_world().expect("enter local world").id, id);

        let (_pick_tx, picks) = crossbeam_channel::unbounded();
        let (events, received) = crossbeam_channel::unbounded();
        let (_command_tx, commands) = crossbeam_channel::unbounded();
        let mut names = crate::names::NameCache::default();
        emit_world_entry(
            &OfflineChannels {
                picks,
                events,
                commands,
            },
            core.snapshot(&id).expect("active snapshot"),
            core.hostiles(),
            Some(&catalog),
            None,
            None,
            &mut names,
        );
        let events: Vec<_> = received.try_iter().collect();
        assert!(
            matches!(events.first(), Some(SessionEvent::Connected { name, .. }) if name == "Alaera")
        );
        assert!(matches!(
            events.get(1),
            Some(SessionEvent::Worldport { map_id: 530, .. })
        ));
        let Some(SessionEvent::ObjectCreate {
            fields, display_id, ..
        }) = events.get(2)
        else {
            panic!("world entry did not emit the player create event: {events:?}");
        };
        assert_eq!(*display_id, catalog.0.body_display(10, 0));
        assert_eq!(
            (
                fields.unit_race(),
                fields.unit_class(),
                fields.unit_gender()
            ),
            (Some(10), Some(8), Some(0))
        );
        assert_eq!(
            (fields.player_skin(), fields.player_face()),
            (Some(0), Some(0))
        );
        assert_eq!(fields.unit_health(), Some(100));
    }

    #[test]
    fn build_8606_practice_hostiles_are_attackable_by_human_and_blood_elf_players() {
        let Some(data) = benilla_formats::wow_data() else {
            eprintln!("skipping: no client install");
            return;
        };
        if !benilla_formats::is_tbc_data_dir(&data) {
            eprintln!("skipping: resolved install is not TBC");
            return;
        }
        let mut chain = benilla_formats::open_chain(&data).expect("open TBC archive chain");
        let factions = crate::target::Factions::from_catalog(
            benilla_formats::load_faction_catalog(&mut chain)
                .expect("load build-8606 faction templates"),
        );
        let reputations = crate::net::Reputations::default();
        for race in [1, 10] {
            let player_faction = factions.race_template(race).expect("race faction template");
            let player = crate::net::ObjectStore(
                ObjectFields::from_pairs(&[(35, player_faction)])
                    .into_created(benilla_protocol::messages::ObjectType::Player),
            );
            let hostile = crate::net::ObjectStore(
                ObjectFields::from_pairs(&[(35, 14)])
                    .into_created(benilla_protocol::messages::ObjectType::Unit),
            );
            assert!(
                crate::target::can_attack(
                    Some(&hostile),
                    Some(&factions),
                    &reputations,
                    Some(&player),
                ),
                "race {race} must see Monster template 14 as hostile"
            );
        }
    }

    #[test]
    fn build_8606_fireball_mana_cost_matches_the_offline_combat_rule() {
        let Some(data) = benilla_formats::wow_data() else {
            eprintln!("skipping: no client install");
            return;
        };
        if !benilla_formats::is_tbc_data_dir(&data) {
            eprintln!("skipping: resolved install is not TBC");
            return;
        }
        let mut chain = benilla_formats::open_chain(&data).expect("open TBC archive chain");
        let spells =
            benilla_formats::load_spell_catalog(&mut chain).expect("load build-8606 spell catalog");
        let fireball = spells
            .get(benilla_game_core::FIREBALL_SPELL_ID)
            .expect("Fireball rank 1");
        assert_eq!(fireball.name, "Fireball");
        assert_eq!(fireball.rank.as_deref(), Some("Rank 1"));
        assert_eq!(fireball.visual, 67);
        assert_eq!(
            fireball.icon.as_deref(),
            Some("Interface\\Icons\\Spell_Fire_FlameBolt")
        );
        assert_eq!(fireball.range_index, 35);
        assert_eq!(fireball.implicit_target_a1, 6);
        assert_eq!(
            crate::spell::cast_target::cast_target_mask(fireball),
            0x0080
        );
        assert_eq!(fireball.school, 2);
        assert_eq!(fireball.prevention_type, 1);
        assert_eq!(fireball.mana_cost, 30);
        assert_eq!(spells.get(168).unwrap().implicit_target_a1, 1);
        assert_eq!(spells.get(1459).unwrap().implicit_target_a1, 21);
        let cast_times = benilla_formats::load_spell_cast_times(&mut chain)
            .expect("load build-8606 spell cast times");
        assert_eq!(
            cast_times
                .get(fireball.casting_time_index)
                .unwrap()
                .resolved_ms(1, fireball.base_level),
            1_500
        );
        let ranges =
            benilla_formats::load_spell_ranges(&mut chain).expect("load build-8606 spell ranges");
        assert_eq!(ranges.get(fireball.range_index).unwrap().max, 35.0);
    }
}
