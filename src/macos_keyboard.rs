use crate::protocol::ModifierFlags;

// macOS 26 requires HIToolbox input-source APIs to run on the main dispatch
// queue. Input capture and forwarding run on worker threads, so use the
// physical key code carried by the wire protocol instead of querying TIS.
pub(crate) struct LayoutTranslator;

impl LayoutTranslator {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn translate(
        &mut self,
        _physical_code: u16,
        _modifiers: &ModifierFlags,
    ) -> Option<String> {
        None
    }
}

pub(crate) fn current_input_source_is_non_english() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::LayoutTranslator;
    use crate::protocol::ModifierFlags;

    #[test]
    fn physical_key_translation_does_not_query_input_source() {
        let mut translator = LayoutTranslator::new();
        assert_eq!(translator.translate(0, &ModifierFlags::default()), None);
    }
}
