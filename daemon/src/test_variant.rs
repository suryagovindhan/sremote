use windows::Win32::System::Variant::{VARIANT, VT_UI4};
use std::mem::ManuallyDrop;

fn main() {
    let mut v = VARIANT::default();
    
    let mut anon = ManuallyDrop::into_inner(v.Anonymous);
    let mut anon2 = ManuallyDrop::into_inner(anon.Anonymous);
    anon2.vt = VT_UI4;
    
    // Check if From works
    let _v2 = VARIANT::from(42u32);
}
