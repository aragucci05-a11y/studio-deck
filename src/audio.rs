// MUTE: per-process mute through the Windows audio mixer (WASAPI sessions on every active output device), the same
// switch as the speaker icon in Volume Mixer. A Studio that has not played a sound yet has no session; the deck
// re-applies a wanted mute on every refresh, so it takes as soon as one appears.
use windows::Win32::{Media::Audio::*, System::Com::*};
use windows::core::Interface;

fn each_session(pid: u32, mut f: impl FnMut(&ISimpleAudioVolume)) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED); // already initialised / other mode: COM still usable
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devs = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..devs.GetCount()? {
            let Ok(mgr) = devs.Item(i)?.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
            let list = mgr.GetSessionEnumerator()?;
            for j in 0..list.GetCount()? {
                let Ok(c2) = list.GetSession(j)?.cast::<IAudioSessionControl2>() else { continue };
                if c2.GetProcessId().ok() == Some(pid) {
                    if let Ok(v) = c2.cast::<ISimpleAudioVolume>() { f(&v) }
                }
            }
        }
    }
    Ok(())
}

pub fn set_muted(pid: u32, on: bool) {
    let _ = each_session(pid, |v| { let _ = unsafe { v.SetMute(on, std::ptr::null()) }; });
}

pub fn is_muted(pid: u32) -> bool {
    let mut any = false;
    let _ = each_session(pid, |v| any |= unsafe { v.GetMute() }.is_ok_and(|b| b.as_bool()));
    any
}
