#[cfg(target_os = "windows")]
mod imp {
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use anyhow::{anyhow, bail, Context, Result};
    use bytes::Bytes;
    use tokio::sync::mpsc;
    use tokio::task;

    use crate::core::{MediaPacket, MediaTrack, NezumiProducer, TrackKind};
    use windows::core::{Interface, IUnknown};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
        ID3D11DeviceContext,
    };
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaSource,
        IMFMediaType, IMFSample, IMFSourceReader, MFCreateAttributes, MFCreateDXGIDeviceManager,
        MFCreateMediaType, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources,
        MFSTARTUP_FULL, MFStartup, MFSampleExtension_CleanPoint,
        MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
        MF_SOURCE_READER_D3D_MANAGER, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
        MFMediaType_Video, MFVideoFormat_HEVC,
    };
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

    const VIDEO_TRACK_ID: u32 = 0;
    const HEVC_CODEC_ID: u8 = 0x01;

    /// Simple wrapper used to mark COM pointers as transferable for the specific usage in this
    /// ingest shim. We only interact with the source reader from one dedicated read thread.
    struct SendCom<T>(T);

    impl<T: Clone> Clone for SendCom<T> {
        fn clone(&self) -> Self {
            Self(self.0.clone())
        }
    }

    impl<T> SendCom<T> {
        fn as_ref(&self) -> &T {
            &self.0
        }
    }

    // SAFETY: This shim serializes access to wrapped COM interfaces and uses an MTA worker thread
    // for the read loop. This is a narrow, deliberate escape hatch to satisfy `spawn_blocking`.
    unsafe impl<T> Send for SendCom<T> {}
    unsafe impl<T> Sync for SendCom<T> {}

    pub struct WindowsMfProducer {
        source_reader: SendCom<IMFSourceReader>,
        _d3d_device: SendCom<ID3D11Device>,
        _d3d_context: SendCom<ID3D11DeviceContext>,
        _dxgi_manager: SendCom<IMFDXGIDeviceManager>,
        stop_flag: Arc<AtomicBool>,
        tracks: Vec<MediaTrack>,
    }

    // SAFETY: Access is via `&mut self` APIs and internal COM interaction is serialized.
    unsafe impl Send for WindowsMfProducer {}
    unsafe impl Sync for WindowsMfProducer {}

    impl Drop for WindowsMfProducer {
        fn drop(&mut self) {
            self.stop_flag.store(true, Ordering::Relaxed);
        }
    }

    impl WindowsMfProducer {
        pub fn new() -> Result<Self> {
            // SAFETY: Media Foundation and most Win32 capture APIs require COM to be initialized on
            // the calling thread. We use the MTA so MF components can operate without STA message
            // pumps and to match the worker read thread apartment model.
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED)
                    .ok()
                    .context("Failed to initialize COM (MTA)")?;
            }

            // SAFETY: MFStartup must be called before any Media Foundation object creation. We use
            // full startup because we need capture, source-reader, and transform support.
            unsafe {
                MFStartup(MF_VERSION, MFSTARTUP_FULL)
                    .context("Failed to startup Media Foundation")?;
            }

            let (d3d_device, d3d_context) = create_d3d11_device()?;
            let dxgi_manager = create_dxgi_device_manager(&d3d_device)?;
            let reader_attrs = create_source_reader_attributes(&dxgi_manager)?;
            let source_reader = create_camera_source_reader(&reader_attrs)?;
            force_hevc_output_on_reader(&source_reader)?;

            Ok(Self {
                source_reader: SendCom(source_reader),
                _d3d_device: SendCom(d3d_device),
                _d3d_context: SendCom(d3d_context),
                _dxgi_manager: SendCom(dxgi_manager),
                stop_flag: Arc::new(AtomicBool::new(false)),
                tracks: vec![MediaTrack {
                    id: VIDEO_TRACK_ID,
                    kind: TrackKind::Video,
                    codec: HEVC_CODEC_ID,
                }],
            })
        }
    }

    fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
        let mut device = None;
        let mut context = None;
        let feature_levels: [D3D_FEATURE_LEVEL; 1] = [D3D_FEATURE_LEVEL_11_0];
        let mut selected_level = D3D_FEATURE_LEVEL_11_0;

        // SAFETY: We pass valid output pointers for the D3D11 device and immediate context. No
        // adapter is provided, so the runtime selects the default hardware adapter.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut selected_level),
                Some(&mut context),
            )
            .context("Failed to create D3D11 device")?;
        }

        let device = device.ok_or_else(|| anyhow!("Failed to create D3D11 device"))?;
        let context = context.ok_or_else(|| anyhow!("Failed to create D3D11 device context"))?;
        let _ = selected_level;
        Ok((device, context))
    }

    fn create_dxgi_device_manager(d3d_device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
        let mut manager = None;
        let mut reset_token = 0u32;

        // SAFETY: Media Foundation allocates and returns an IMFDXGIDeviceManager through the out
        // parameter and also returns a reset token required for ResetDevice.
        unsafe {
            MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)
                .context("Failed to create DXGI device manager")?;
        }

        let manager = manager.ok_or_else(|| anyhow!("Failed to create DXGI device manager"))?;
        let d3d_unknown: IUnknown = d3d_device
            .cast()
            .context("Failed to cast D3D11 device for DXGI manager binding")?;

        // SAFETY: `reset_token` came from `MFCreateDXGIDeviceManager`, and `d3d_unknown` points to
        // a live D3D11 device. This binds the device to MF for zero-copy DXGI surface sharing.
        unsafe {
            manager
                .ResetDevice(&d3d_unknown, reset_token)
                .context("Failed to bind DXGI Manager")?;
        }

        Ok(manager)
    }

    fn create_source_reader_attributes(
        dxgi_manager: &IMFDXGIDeviceManager,
    ) -> Result<IMFAttributes> {
        let mut attrs = None;

        // SAFETY: We request a small attribute store for source reader configuration flags.
        unsafe {
            MFCreateAttributes(&mut attrs, 4).context("Failed to create source reader attributes")?;
        }

        let attrs = attrs.ok_or_else(|| anyhow!("Failed to create source reader attributes"))?;
        let dxgi_unknown: IUnknown = dxgi_manager
            .cast()
            .context("Failed to cast DXGI manager for source reader attributes")?;

        // SAFETY: Attaching `MF_SOURCE_READER_D3D_MANAGER` allows MF to use GPU-backed samples;
        // enabling hardware transforms permits automatic insertion of hardware MFTs (like HEVC).
        unsafe {
            attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &dxgi_unknown)
                .context("Failed to set MF_SOURCE_READER_D3D_MANAGER")?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .context("Failed to enable hardware transforms on source reader")?;
        }

        Ok(attrs)
    }

    fn create_camera_source_reader(reader_attrs: &IMFAttributes) -> Result<IMFSourceReader> {
        let activates = enumerate_video_capture_devices()?;
        let first = activates
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No local video capture devices found"))?;

        // SAFETY: The activation object was returned by MF device enumeration for a video capture
        // source; activating it yields an IMFMediaSource for the selected camera.
        let media_source: IMFMediaSource = unsafe {
            first
                .ActivateObject()
                .context("Failed to activate the first video capture device")?
        };

        // SAFETY: We pass a valid media source and the configured source-reader attributes (D3D
        // manager + hardware transforms). MF constructs and returns the source reader COM object.
        unsafe { MFCreateSourceReaderFromMediaSource(&media_source, Some(reader_attrs)) }
            .context("Failed to create IMFSourceReader from the video capture device")
    }

    fn force_hevc_output_on_reader(source_reader: &IMFSourceReader) -> Result<()> {
        // SAFETY: MFCreateMediaType allocates a mutable media-type object used to request HEVC
        // output from the source reader graph (which may auto-insert an encoder MFT).
        let hevc_type: IMFMediaType =
            unsafe { MFCreateMediaType() }.context("Failed to create HEVC IMFMediaType")?;

        // SAFETY: We are configuring the media type we just created. Setting major type + subtype
        // to Video/HEVC requests encoded HEVC output rather than raw camera frames.
        unsafe {
            hevc_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .context("Failed to set HEVC media type major type")?;
            hevc_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)
                .context("Failed to set HEVC media type subtype")?;
            source_reader
                .SetCurrentMediaType(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    None,
                    Some(&hevc_type),
                )
                .context(
                    "Failed to configure HEVC output on source reader (camera/MF hardware HEVC encoder may not be available)",
                )?;
        }

        Ok(())
    }

    fn enumerate_video_capture_devices() -> Result<Vec<IMFActivate>> {
        let mut enum_attrs = None;

        // SAFETY: This attribute store is used only to describe the device enumeration query.
        unsafe {
            MFCreateAttributes(&mut enum_attrs, 1)
                .context("Failed to create MF enumeration attributes")?;
        }

        let enum_attrs =
            enum_attrs.ok_or_else(|| anyhow!("Failed to create MF enumeration attributes"))?;

        // SAFETY: Restrict enumeration to local video capture devices (webcams, capture cards).
        unsafe {
            enum_attrs
                .SetGUID(
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
                )
                .context("Failed to configure video device enumeration attributes")?;
        }

        let mut raw_devices: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;

        // SAFETY: MF allocates a COM task-memory array of activation objects and returns ownership
        // to the caller. We must copy/clone the COM interfaces we want and free the array memory.
        unsafe {
            MFEnumDeviceSources(&enum_attrs, &mut raw_devices, &mut count)
                .context("Failed to enumerate local video capture devices")?;
        }

        if count == 0 {
            return Ok(Vec::new());
        }
        if raw_devices.is_null() {
            bail!("MFEnumDeviceSources returned a null device list");
        }

        let mut devices = Vec::with_capacity(count as usize);

        // SAFETY: `raw_devices` is a valid `count`-element array allocated with CoTaskMem.
        // We clone each activation interface into Rust-owned COM wrappers and then free the array.
        unsafe {
            let slice = std::slice::from_raw_parts(raw_devices, count as usize);
            for activate in slice {
                if let Some(activate) = activate.clone() {
                    devices.push(activate);
                }
            }
            CoTaskMemFree(Some(raw_devices.cast()));
        }

        Ok(devices)
    }

    fn read_sample_packet(source_reader: &IMFSourceReader, track: &MediaTrack) -> Result<Option<MediaPacket>> {
        let mut actual_stream_index = 0u32;
        let mut stream_flags = 0u32;
        let mut timestamp_hns = 0i64;
        let mut sample: Option<IMFSample> = None;

        // SAFETY: `ReadSample` writes into the provided out-pointers and returns an IMFSample when
        // a frame is available. We read from the first video stream configured in `new()`.
        unsafe {
            source_reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    Some(&mut actual_stream_index),
                    Some(&mut stream_flags),
                    Some(&mut timestamp_hns),
                    Some(&mut sample),
                )
                .context("Failed to read sample from Media Foundation source reader")?;
        }

        let Some(sample) = sample else {
            let _ = actual_stream_index;
            let _ = stream_flags;
            let _ = timestamp_hns;
            return Ok(None);
        };

        let pts = sample_time_hns(&sample).unwrap_or_else(|_| timestamp_hns.max(0) as u64);
        let payload = sample_to_bytes(&sample)?;
        let is_keyframe = sample_is_keyframe(&sample);

        Ok(Some(MediaPacket {
            track_id: track.id,
            pts,
            is_keyframe,
            payload,
        }))
    }

    fn sample_time_hns(sample: &IMFSample) -> Result<u64> {
        // SAFETY: `sample` is a live COM object returned by MF. `GetSampleTime` writes a 100ns
        // timestamp into the provided output pointer.
        let sample_time = unsafe { sample.GetSampleTime() }
            .context("Failed to read IMFSample sample time")?;

        Ok(sample_time.max(0) as u64)
    }

    fn sample_is_keyframe(sample: &IMFSample) -> bool {
        // SAFETY: `GetUINT32` only reads sample metadata. If the clean-point attribute is absent,
        // we conservatively treat the sample as non-keyframe to avoid over-reporting.
        unsafe {
            sample
                .GetUINT32(&MFSampleExtension_CleanPoint)
                .map(|v| v != 0)
                .unwrap_or(false)
        }
    }

    fn sample_to_bytes(sample: &IMFSample) -> Result<Bytes> {
        // SAFETY: This requests a single contiguous buffer view for the sample payload. MF either
        // returns the existing buffer or copies scatter/gather buffers into a temporary contiguous
        // IMFMediaBuffer managed by COM.
        let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer() }
            .context("Failed to convert IMFSample to contiguous buffer")?;

        let mut raw_ptr: *mut u8 = ptr::null_mut();
        let mut max_len = 0u32;
        let mut current_len = 0u32;

        // SAFETY: `Lock` returns a stable pointer valid until `Unlock` is called. We immediately
        // copy the bytes into an owned `Bytes` buffer and then unlock before returning.
        unsafe {
            buffer
                .Lock(&mut raw_ptr, Some(&mut max_len), Some(&mut current_len))
                .context("Failed to lock IMFMediaBuffer")?;
        }

        if raw_ptr.is_null() && current_len != 0 {
            // SAFETY: The buffer was successfully locked above and must be unlocked before erroring.
            unsafe {
                let _ = buffer.Unlock();
            }
            bail!("IMFMediaBuffer::Lock returned null data pointer");
        }

        let data = if current_len == 0 {
            Bytes::new()
        } else {
            // SAFETY: `raw_ptr` is valid for `current_len` bytes until `Unlock`.
            let slice = unsafe { std::slice::from_raw_parts(raw_ptr.cast_const(), current_len as usize) };
            Bytes::copy_from_slice(slice)
        };

        let _ = max_len;

        // SAFETY: Must pair with a successful `Lock` call above to release the mapped pointer.
        unsafe {
            buffer
                .Unlock()
                .context("Failed to unlock IMFMediaBuffer")?;
        }

        Ok(data)
    }

    #[async_trait::async_trait]
    impl NezumiProducer for WindowsMfProducer {
        fn tracks(&self) -> Vec<MediaTrack> {
            self.tracks.clone()
        }

        async fn start_reading(&mut self, sender: mpsc::UnboundedSender<MediaPacket>) -> Result<()> {
            let source_reader = self.source_reader.clone();
            let stop_flag = Arc::clone(&self.stop_flag);
            let track = self
                .tracks
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("WindowsMfProducer has no video track configured"))?;

            let handle = task::spawn_blocking(move || -> Result<()> {
                // SAFETY: The blocking read loop runs on a Tokio worker thread. We initialize COM in
                // MTA mode on that thread before issuing any MF COM calls.
                unsafe {
                    CoInitializeEx(None, COINIT_MULTITHREADED)
                        .ok()
                        .context("Failed to initialize COM on MF read thread")?;
                }

                loop {
                    if stop_flag.load(Ordering::Relaxed) {
                        return Ok(());
                    }

                    if sender.is_closed() {
                        return Ok(());
                    }

                    if let Some(packet) = read_sample_packet(source_reader.as_ref(), &track)? {
                        // A closed receiver is treated as a normal shutdown request so main can
                        // stop capture after the 10-second analysis window.
                        if sender.send(packet).is_err() {
                            return Ok(());
                        }
                    }
                }
            });

            handle
                .await
                .context("Windows MF blocking read thread join failure")?
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use anyhow::{bail, Result};
    use tokio::sync::mpsc;

    use crate::core::{MediaPacket, MediaTrack, NezumiProducer};

    pub struct WindowsMfProducer;

    impl WindowsMfProducer {
        pub fn new() -> Result<Self> {
            bail!("WindowsMfProducer is only available on Windows")
        }
    }

    #[async_trait::async_trait]
    impl NezumiProducer for WindowsMfProducer {
        fn tracks(&self) -> Vec<MediaTrack> {
            Vec::new()
        }

        async fn start_reading(&mut self, _sender: mpsc::UnboundedSender<MediaPacket>) -> Result<()> {
            bail!("WindowsMfProducer is only available on Windows")
        }
    }
}

pub use imp::WindowsMfProducer;
