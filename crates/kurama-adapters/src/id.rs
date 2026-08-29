use kurama_protocol::{
    id::{AgentId, CallId, OperationId, SessionId},
    traits::IdGenerator,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct RandomIds;

impl RandomIds {
    fn next(prefix: &str) -> String {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).expect("operating system randomness unavailable");

        let mut output = String::with_capacity(34);
        output.push_str(prefix);
        for byte in bytes {
            output.push(nibble(byte >> 4));
            output.push(nibble(byte & 0x0f));
        }
        output
    }
}

impl IdGenerator for RandomIds {
    fn session_id(&self) -> SessionId {
        Self::next("s_").into()
    }

    fn agent_id(&self) -> AgentId {
        Self::next("a_").into()
    }

    fn operation_id(&self) -> OperationId {
        Self::next("o_").into()
    }

    fn call_id(&self) -> CallId {
        Self::next("c_").into()
    }
}

fn nibble(value: u8) -> char {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    char::from(DIGITS[value as usize])
}
