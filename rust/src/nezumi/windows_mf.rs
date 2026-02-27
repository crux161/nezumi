#[cfg(target_os = "windows")]
mod imp {
    use std::ptr;

    use anyhow::{anyhow, bail, Context, Result};
    use bytes::Bytes;
    use tokio::sync::mpsc;

    use crate::nezumi::core::{MediaPacket, MediaTrack, NezumiProducer, TrackKind};
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
        IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaSource, IMFSourceReader,
        MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_D3D_MANAGER,
        MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION, MFCreateAttributes,
        MFCreateDXGIDeviceManager, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources,
        MFSTARTUP_FULL, MFStartup,
    };
    use windows::Win32::System::Com::{
        CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED,
    };

    const DUMMY_CODEC_ID: u8 = 0;
    const VIDEO_TRACK_ID: u32 = 0;

    pub struct WindowsMfProducer {
        source_reader: IMFSourceReader,
        _d3d_device: ID3D11Device,
        _d3d_context: ID3D11DeviceContext,
        _dxgi_manager: IMFDXGIDeviceManager,
        tracks: Vec<MediaTrack>,
    }

    // SAFETY: `windows` COM interface wrappers in `windows` 0.58 do not implement `Send`/`Sync`
    // automatically. This producer is designed for serialized access through `&mut self` and keeps
    // the COM graph alive for the lifetime of the producer. The Nezumi trait requires `Send + Sync`,
    // so we opt into those bounds explicitly for this scaffold.
    unsafe impl Send for WindowsMfProducer {}
    unsafe impl Sync for WindowsMfProducer {}

    impl WindowsMfProducer {
        pub fn new() -> Result<Self> {
            // SAFETY: We initialize COM for the current thread using the MTA model before creating
            // any COM-based Media Foundation objects. This is required for MF and DXGI COM usage.
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED)
                    .ok()
                    .context("Failed to initialize COM (MTA)")?;
            }

            // SAFETY: MFStartup must be called once per process before using Media Foundation APIs.
            // We request the full startup mode because we need capture + source reader functionality.
            unsafe {
                MFStartup(MF_VERSION, MFSTARTUP_FULL).context("Failed to startup Media Foundation")?;
            }

            let (d3d_device, d3d_context) = create_d3d11_device()?;
            let dxgi_manager = create_dxgi_device_manager(&d3d_device)?;
            let source_reader_attrs = create_source_reader_attributes(&dxgi_manager)?;
            let source_reader = create_camera_source_reader(&source_reader_attrs)?;

            Ok(Self {
                source_reader,
                _d3d_device: d3d_device,
                _d3d_context: d3d_context,
                _dxgi_manager: dxgi_manager,
                tracks: vec![MediaTrack {
                    id: VIDEO_TRACK_ID,
                    kind: TrackKind::Video,
                    codec: DUMMY_CODEC_ID,
                }],
            })
        }
    }

    fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut chosen_feature_level = D3D_FEATURE_LEVEL_11_0;
        let feature_levels: [D3D_FEATURE_LEVEL; 1] = [D3D_FEATURE_LEVEL_11_0];

        // SAFETY: We provide valid out-pointers for the D3D11 device/context and request a
        // hardware device. No adapter handle is supplied, so DXGI chooses the default adapter.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut chosen_feature_level),
                Some(&mut context),
            )
            .context("Failed to create D3D11 device")?;
        }

        let device = device.ok_or_else(|| anyhow!("Failed to create D3D11 device"))?;
        let context = context.ok_or_else(|| anyhow!("Failed to create D3D11 device context"))?;
        let _ = chosen_feature_level;

        Ok((device, context))
    }

    fn create_dxgi_device_manager(d3d_device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
        let mut reset_token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;

        // SAFETY: We pass valid out-pointers for the reset token and COM interface. MF allocates
        // the device manager and initializes it for DXGI interop.
        unsafe {
            MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)
                .context("Failed to create DXGI device manager")?;
        }

        let manager = manager.ok_or_else(|| anyhow!("Failed to create DXGI device manager"))?;
        let d3d_unknown: IUnknown = d3d_device
            .cast()
            .context("Failed to cast D3D11 device for DXGI manager binding")?;

        // SAFETY: The device manager was just created and the reset token came from
        // MFCreateDXGIDeviceManager. We bind the live D3D11 device so MF can hand us DXGI-backed
        // video samples (zero-copy surfaces) through the source reader path.
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
        let mut attrs: Option<IMFAttributes> = None;

        // SAFETY: We request an attribute store with enough capacity for the two required keys.
        unsafe {
            MFCreateAttributes(&mut attrs, 2).context("Failed to create source reader attributes")?;
        }

        let attrs = attrs.ok_or_else(|| anyhow!("Failed to create source reader attributes"))?;
        let dxgi_unknown: IUnknown = dxgi_manager
            .cast()
            .context("Failed to cast DXGI manager for source reader attributes")?;

        // SAFETY: We attach the DXGI device manager COM object to the source reader so MF can
        // produce GPU-backed video surfaces, and we explicitly allow hardware transforms.
        unsafe {
            attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &dxgi_unknown)
                .context("Failed to set MF_SOURCE_READER_D3D_MANAGER")?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .context("Failed to enable hardware transforms for source reader")?;
        }

        Ok(attrs)
    }

    fn create_camera_source_reader(reader_attrs: &IMFAttributes) -> Result<IMFSourceReader> {
        let activates = enumerate_video_capture_devices()?;
        let first = activates
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No local video capture devices found"))?;

        // SAFETY: IMFActivate::ActivateObject creates the actual IMFMediaSource for the selected
        // camera. The activation object came from MFEnumDeviceSources.
        let media_source: IMFMediaSource = unsafe {
            first
                .ActivateObject()
                .context("Failed to activate first video capture device")?
        };

        // SAFETY: The media source is a live capture source and reader_attrs contains the DXGI
        // manager + hardware transform settings. MF initializes the source reader for capture.
        unsafe { MFCreateSourceReaderFromMediaSource(&media_source, Some(reader_attrs)) }
            .context("Failed to create IMFSourceReader from video capture device")
    }

    fn enumerate_video_capture_devices() -> Result<Vec<IMFActivate>> {
        let mut enum_attrs: Option<IMFAttributes> = None;

        // SAFETY: We allocate an attribute store used only to describe the device-source query.
        unsafe {
            MFCreateAttributes(&mut enum_attrs, 1)
                .context("Failed to create MF device enumeration attributes")?;
        }

        let enum_attrs =
            enum_attrs.ok_or_else(|| anyhow!("Failed to create MF device enumeration attributes"))?;

        // SAFETY: We set the required "video capture source" attribute so MFEnumDeviceSources
        // returns local camera devices rather than unrelated MF sources.
        unsafe {
            enum_attrs
                .SetGUID(
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
                )
                .context("Failed to configure video capture device enumeration")?;
        }

        let mut raw_activates: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;

        // SAFETY: MF allocates a CoTaskMem block containing `count` IMFActivate interface slots.
        // We copy clones of the COM interfaces into a Rust Vec and then free the original block.
        unsafe {
            MFEnumDeviceSources(&enum_attrs, &mut raw_activates, &mut count)
                .context("Failed to enumerate local video capture devices")?;
        }

        if count == 0 {
            return Ok(Vec::new());
        }

        if raw_activates.is_null() {
            bail!("MFEnumDeviceSources returned a null device list");
        }

        let mut devices = Vec::with_capacity(count as usize);

        // SAFETY: `raw_activates` points to `count` entries allocated by MFEnumDeviceSources.
        // Each entry is an optional COM interface. We clone the valid interfaces before freeing
        // the original array memory with CoTaskMemFree.
        unsafe {
            let slice = std::slice::from_raw_parts(raw_activates, count as usize);
            for item in slice {
                if let Some(activate) = item.clone() {
                    devices.push(activate);
                }
            }
            CoTaskMemFree(Some(raw_activates.cast()));
        }

        Ok(devices)
    }

    #[async_trait::async_trait]
    impl NezumiProducer for WindowsMfProducer {
        fn tracks(&self) -> Vec<MediaTrack> {
            self.tracks.clone()
        }

        async fn start_reading(&mut self, sender: mpsc::UnboundedSender<MediaPacket>) -> Result<()> {
            let track = self
                .tracks
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("WindowsMfProducer has no configured tracks"))?;
            let video_stream_index = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

            loop {
                let mut actual_stream_index = 0u32;
                let mut stream_flags = 0u32;
                let mut timestamp_hns = 0i64;
                let mut sample = None;

                // SAFETY: We call ReadSample on the configured source reader with valid
                // out-pointers for stream index, flags, timestamp, and IMFSample. The stream index
                // is the Media Foundation constant for the first video stream.
                unsafe {
                    self.source_reader
                        .ReadSample(
                            video_stream_index,
                            0,
                            Some(&mut actual_stream_index),
                            Some(&mut stream_flags),
                            Some(&mut timestamp_hns),
                            Some(&mut sample),
                        )
                        .context("Failed to read sample from Media Foundation source reader")?;
                }

                let Some(_sample) = sample else {
                    // Live capture readers may occasionally return no sample (for example a
                    // stream tick). Continue reading until the receiver disconnects or MF fails.
                    let _ = actual_stream_index;
                    let _ = stream_flags;
                    continue;
                };

                let packet = MediaPacket {
                    track_id: track.id,
                    pts: timestamp_hns.max(0) as u64,
                    codec: track.codec,
                    is_keyframe: true,
                    // Dummy packet for now. We only prove capture->timestamp flow in this phase;
                    // HEVC encoding and real payload extraction will be added later.
                    payload: Bytes::new(),
                };

                sender
                    .send(packet)
                    .map_err(|_| anyhow!("Media packet receiver dropped"))?;
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use anyhow::{bail, Result};
    use tokio::sync::mpsc;

    use crate::nezumi::core::{MediaPacket, MediaTrack, NezumiProducer};

    pub struct WindowsMfProducer;

    impl WindowsMfProducer {
        pub fn new() -> Result<Self> {
            bail!("WindowsMfProducer is only available on Windows");
        }
    }

    #[async_trait::async_trait]
    impl NezumiProducer for WindowsMfProducer {
        fn tracks(&self) -> Vec<MediaTrack> {
            Vec::new()
        }

        async fn start_reading(&mut self, _sender: mpsc::UnboundedSender<MediaPacket>) -> Result<()> {
            bail!("WindowsMfProducer is only available on Windows");
        }
    }
}

pub use imp::WindowsMfProducer;
