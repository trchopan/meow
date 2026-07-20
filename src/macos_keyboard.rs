use crate::protocol::ModifierFlags;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::ffi::c_void;

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
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
        fn CFRelease(value: *const c_void);
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
}

pub(crate) use imp::LayoutTranslator;
