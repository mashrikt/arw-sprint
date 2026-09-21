//! Pure shortcut routing; deciding whether a key requests a move performs no I/O.

use winit::keyboard::{Key, ModifiersState, NamedKey};

pub(super) fn move_requested(key: &Key, modifiers: ModifiersState, repeat: bool) -> bool {
    if repeat || modifiers.super_key() || modifiers.control_key() || modifiers.alt_key() {
        return false;
    }
    match key.as_ref() {
        Key::Named(NamedKey::Space) | Key::Character(" ") => !modifiers.shift_key(),
        Key::Character("x" | "X") => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn move_keys() -> [Key; 4] {
        [
            Key::Named(NamedKey::Space),
            Key::Character(" ".into()),
            Key::Character("x".into()),
            Key::Character("X".into()),
        ]
    }

    #[test]
    fn space_representations_and_both_x_cases_request_one_move() {
        for key in move_keys() {
            assert!(
                move_requested(&key, ModifiersState::empty(), false),
                "{key:?}"
            );
        }
    }

    #[test]
    fn shift_space_remains_navigation_while_shift_x_can_move() {
        for key in move_keys().into_iter().take(2) {
            assert!(
                !move_requested(&key, ModifiersState::SHIFT, false),
                "{key:?}"
            );
        }
        for key in move_keys().into_iter().skip(2) {
            assert!(
                move_requested(&key, ModifiersState::SHIFT, false),
                "{key:?}"
            );
        }
    }

    #[test]
    fn held_move_keys_never_request_another_move() {
        for key in move_keys() {
            for modifiers in [ModifiersState::empty(), ModifiersState::SHIFT] {
                assert!(
                    !move_requested(&key, modifiers, true),
                    "{key:?} {modifiers:?}"
                );
            }
        }
    }

    #[test]
    fn every_command_control_option_combination_blocks_moves() {
        for bits in 1u8..16 {
            let mut modifiers = ModifiersState::empty();
            for (index, flag) in [
                ModifiersState::SUPER,
                ModifiersState::CONTROL,
                ModifiersState::ALT,
                ModifiersState::SHIFT,
            ]
            .into_iter()
            .enumerate()
            {
                if bits & (1 << index) != 0 {
                    modifiers |= flag;
                }
            }
            if modifiers == ModifiersState::SHIFT {
                continue;
            }
            for key in move_keys() {
                for repeat in [false, true] {
                    assert!(
                        !move_requested(&key, modifiers, repeat),
                        "{key:?} {modifiers:?} repeat={repeat}"
                    );
                }
            }
        }
    }

    #[test]
    fn unrelated_keys_and_character_sequences_never_request_moves() {
        let unrelated = [
            Key::Named(NamedKey::ArrowRight),
            Key::Named(NamedKey::ArrowLeft),
            Key::Named(NamedKey::Tab),
            Key::Named(NamedKey::Enter),
            Key::Dead(Some('x')),
            Key::Character("".into()),
            Key::Character("xx".into()),
            Key::Character("X ".into()),
            Key::Character("Space".into()),
            Key::Character("χ".into()),
            Key::Character("s".into()),
            Key::Character("d".into()),
        ];
        for key in unrelated {
            for modifiers in [ModifiersState::empty(), ModifiersState::SHIFT] {
                assert!(
                    !move_requested(&key, modifiers, false),
                    "{key:?} {modifiers:?}"
                );
            }
        }
    }
}
