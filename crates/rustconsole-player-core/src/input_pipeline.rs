use rustconsole_protocol::InputEvent;
use rustconsole_protocol::wire::{
    InputTransition, KeyTransition, PointerButtonTransition, PointerMotionTransition,
    PointerPositionTransition, ReleaseAll, WheelTransition, input_transition,
};

#[must_use]
pub fn encode_reliable_input(
    event: InputEvent,
    generation: u64,
    sequence: u64,
    player_sent_at_micros: u64,
) -> InputTransition {
    let action = match event {
        InputEvent::Key { hid_usage, pressed } => input_transition::Action::Key(KeyTransition {
            hid_usage: u32::from(hid_usage),
            pressed,
        }),
        InputEvent::ReleaseAll => input_transition::Action::ReleaseAll(ReleaseAll {}),
        InputEvent::PointerButton { button, pressed } => {
            input_transition::Action::PointerButton(PointerButtonTransition {
                button: u32::from(button),
                pressed,
            })
        }
        InputEvent::PointerMotion { delta_x, delta_y } => {
            input_transition::Action::PointerMotion(PointerMotionTransition { delta_x, delta_y })
        }
        InputEvent::PointerPosition { x, y } => {
            input_transition::Action::PointerPosition(PointerPositionTransition {
                x: u32::from(x),
                y: u32::from(y),
            })
        }
        InputEvent::Wheel {
            horizontal,
            vertical,
        } => input_transition::Action::Wheel(WheelTransition {
            horizontal: i32::from(horizontal),
            vertical: i32::from(vertical),
        }),
    };
    InputTransition {
        generation,
        sequence,
        action: Some(action),
        player_sent_at_micros,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use rustconsole_protocol::wire::InputPack;

    #[test]
    fn every_local_input_has_a_reliable_wire_action() {
        let events = [
            InputEvent::Key {
                hid_usage: 4,
                pressed: true,
            },
            InputEvent::ReleaseAll,
            InputEvent::PointerButton {
                button: 1,
                pressed: true,
            },
            InputEvent::PointerMotion {
                delta_x: -4,
                delta_y: 8,
            },
            InputEvent::PointerPosition { x: 10, y: 20 },
            InputEvent::Wheel {
                horizontal: -1,
                vertical: 2,
            },
        ];
        for (index, event) in events.into_iter().enumerate() {
            let transition = encode_reliable_input(event, 3, index as u64 + 1, 50);
            assert_eq!(transition.generation, 3);
            assert_eq!(transition.sequence, index as u64 + 1);
            assert!(transition.action.is_some());
        }
    }

    #[test]
    fn local_input_survives_a_reliable_wire_round_trip() {
        let expected = encode_reliable_input(
            InputEvent::PointerMotion {
                delta_x: -17,
                delta_y: 23,
            },
            4,
            9,
            12_345,
        );
        let encoded = InputPack {
            transitions: vec![expected.clone()],
        }
        .encode_to_vec();
        let decoded = InputPack::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.transitions, [expected]);
    }
}
