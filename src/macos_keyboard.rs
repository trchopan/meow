use crate::protocol::ModifierFlags;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::ffi::{CStr, c_void};

    type InputSourceRef = *mut c_void;
    type OptionBits = u32;

    const KEY_ACTION_DOWN: u16 = 0;
    const DEAD_KEYS: OptionBits = 1 << 31;
    const MAX_CHARS: usize = 8;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> InputSourceRef;
        fn TISCopyCurrentKeyboardLayoutInputSource() -> InputSourceRef;
        fn TISGetInputSourceProperty(
            source: InputSourceRef,
            property: *mut c_void,
        ) -> *const c_void;
        static kTISPropertyInputSourceLanguages: *mut c_void;
        static kTISPropertyUnicodeKeyLayoutData: *mut c_void;
        fn UCKeyTranslate(
            layout: *const u8,
            virtual_key: u16,
            action: u16,
            modifiers: u32,
            keyboard_type: u32,
            options: OptionBits,
            dead_key_state: *mut u32,
            max_length: usize,
            actual_length: *mut usize,
            output: *mut u16,
        ) -> i32;
        fn LMGetKbdType() -> u32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut i8,
            buffer_size: isize,
            encoding: u32,
        ) -> bool;
        fn CFRelease(value: *const c_void);
    }

    const K_CFSTRING_ENCODING_UTF8: u32 = 0x0800_0100;

    fn is_english_language(language: &str) -> bool {
        language
            .get(..2)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("en"))
    }

    pub(crate) struct LayoutTranslator {
        dead_key_state: u32,
    }

    impl LayoutTranslator {
        pub(crate) fn new() -> Self {
            Self { dead_key_state: 0 }
        }

        pub(crate) fn translate(
            &mut self,
            physical_code: u16,
            modifiers: &ModifierFlags,
        ) -> Option<String> {
            let source = unsafe { TISCopyCurrentKeyboardInputSource() };
            let mut layout =
                unsafe { TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData) };
            if layout.is_null() {
                let fallback = unsafe { TISCopyCurrentKeyboardLayoutInputSource() };
                if !source.is_null() {
                    unsafe { CFRelease(source as *const c_void) };
                }
                layout = unsafe {
                    TISGetInputSourceProperty(fallback, kTISPropertyUnicodeKeyLayoutData)
                };
                if layout.is_null() {
                    if !fallback.is_null() {
                        unsafe { CFRelease(fallback as *const c_void) };
                    }
                    return None;
                }
                let result = self.translate_with_layout(layout, physical_code, modifiers);
                unsafe { CFRelease(fallback as *const c_void) };
                return result;
            }

            let result = self.translate_with_layout(layout, physical_code, modifiers);
            if !source.is_null() {
                unsafe { CFRelease(source as *const c_void) };
            }
            result
        }

        fn translate_with_layout(
            &mut self,
            layout: *const c_void,
            physical_code: u16,
            modifiers: &ModifierFlags,
        ) -> Option<String> {
            let layout_bytes = unsafe { CFDataGetBytePtr(layout) };
            if layout_bytes.is_null() {
                return None;
            }
            let mut output = [0u16; MAX_CHARS];
            let mut length = 0usize;
            let status = unsafe {
                UCKeyTranslate(
                    layout_bytes,
                    physical_code,
                    KEY_ACTION_DOWN,
                    modifier_state(modifiers),
                    LMGetKbdType(),
                    DEAD_KEYS,
                    &mut self.dead_key_state,
                    MAX_CHARS,
                    &mut length,
                    output.as_mut_ptr(),
                )
            };
            if status != 0 || length == 0 {
                return None;
            }
            String::from_utf16(&output[..length]).ok()
        }
    }

    fn modifier_state(modifiers: &ModifierFlags) -> u32 {
        let mut state = 0;
        if modifiers.left_shift || modifiers.right_shift {
            state |= 1 << 1;
        }
        if modifiers.left_control || modifiers.right_control {
            state |= 1 << 4;
        }
        if modifiers.left_alt || modifiers.right_alt {
            state |= 1 << 3;
        }
        if modifiers.left_meta || modifiers.right_meta {
            state |= 1;
        }
        state
    }

    pub(crate) fn current_input_source_is_non_english() -> bool {
        let current = unsafe { TISCopyCurrentKeyboardInputSource() };
        if current.is_null() {
            return false;
        }

        let languages =
            unsafe { TISGetInputSourceProperty(current, kTISPropertyInputSourceLanguages) };
        let mut non_english = false;
        if !languages.is_null() {
            let count = unsafe { CFArrayGetCount(languages) };
            for index in 0..count {
                let language = unsafe { CFArrayGetValueAtIndex(languages, index) };
                if language.is_null() {
                    continue;
                }
                let mut buffer = [0i8; 16];
                let is_utf8 = unsafe {
                    CFStringGetCString(
                        language,
                        buffer.as_mut_ptr(),
                        buffer.len() as isize,
                        K_CFSTRING_ENCODING_UTF8,
                    )
                };
                if is_utf8
                    && let Ok(language) = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_str()
                    && !is_english_language(language)
                {
                    non_english = true;
                    break;
                }
            }
        }
        unsafe {
            CFRelease(current as *const c_void);
        }
        non_english
    }

    #[cfg(test)]
    mod tests {
        use super::is_english_language;

        #[test]
        fn english_language_tags_are_detected() {
            assert!(is_english_language("en"));
            assert!(is_english_language("en-US"));
            assert!(is_english_language("EN_gb"));
            assert!(!is_english_language("vi"));
            assert!(!is_english_language("zh-Hans"));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

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
}

pub(crate) use imp::{LayoutTranslator, current_input_source_is_non_english};
