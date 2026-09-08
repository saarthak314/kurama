use kurama_protocol::{KuramaError, runtime::RuntimeEvent, traits::EventSink};

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _event: RuntimeEvent) -> Result<(), KuramaError> {
        Ok(())
    }
}
