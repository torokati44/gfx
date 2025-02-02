/*!
# Vulkan backend internals.

## Stack memory

Most of the code just passes the data through. The only problem
that affects all the pieces is related to memory allocation:
Vulkan expects slices, but the API gives us `ExactSizeIterator`.
So we end up using a lot of `inplace_it` to get things collected on stack.

## Framebuffers

One part that has actual logic is related to framebuffers. HAL is modelled
after image-less framebuffers. If the the Vulkan implementation supports it,
we map it 1:1, and everything is great. If it doesn't expose
`KHR_imageless_framebuffer`, however, than we have to keep all the created
framebuffers internally in an internally-synchronized map, per `B::Framebuffer`.
!*/

#![allow(non_snake_case)]

#[macro_use]
extern crate log;

#[cfg(target_os = "macos")]
#[macro_use]
extern crate objc;

#[cfg(not(feature = "use-rtld-next"))]
use ash::Entry;
use ash::{
    extensions::{
        self,
        ext::{DebugReport, DebugUtils},
        khr::DrawIndirectCount,
        khr::Swapchain,
        nv::MeshShader,
    },
    version::{DeviceV1_0, EntryV1_0, InstanceV1_0},
    vk,
};

use hal::{
    adapter,
    device::{CreationError as DeviceCreationError, DeviceLost, OutOfMemory},
    format, image, memory,
    pso::{PatchSize, PipelineStage},
    queue,
    window::{OutOfDate, PresentError, Suboptimal, SurfaceLost},
    Features, Hints, Limits,
};

use std::{
    borrow::{Borrow, Cow},
    ffi::{CStr, CString},
    fmt, mem, slice,
    sync::Arc,
    thread,
};

#[cfg(feature = "use-rtld-next")]
use ash::EntryCustom;
#[cfg(feature = "use-rtld-next")]
use shared_library::dynamic_library::{DynamicLibrary, SpecialHandles};

mod command;
mod conv;
mod device;
mod info;
mod native;
mod pool;
mod window;

// Sets up the maximum count we expect in most cases, but maybe not all of them.
const ROUGH_MAX_ATTACHMENT_COUNT: usize = 5;

pub struct RawInstance {
    inner: ash::Instance,
    debug_messenger: Option<DebugMessenger>,
    get_physical_device_properties: Option<vk::KhrGetPhysicalDeviceProperties2Fn>,
}

pub enum DebugMessenger {
    Utils(DebugUtils, vk::DebugUtilsMessengerEXT),
    Report(DebugReport, vk::DebugReportCallbackEXT),
}

impl Drop for RawInstance {
    fn drop(&mut self) {
        unsafe {
            match self.debug_messenger {
                Some(DebugMessenger::Utils(ref ext, callback)) => {
                    ext.destroy_debug_utils_messenger(callback, None)
                }
                Some(DebugMessenger::Report(ref ext, callback)) => {
                    ext.destroy_debug_report_callback(callback, None)
                }
                None => {}
            }
            self.inner.destroy_instance(None);
        }
    }
}

pub struct Instance {
    pub raw: Arc<RawInstance>,

    /// Supported extensions of this instance.
    pub extensions: Vec<&'static CStr>,

    #[cfg(not(feature = "use-rtld-next"))]
    pub entry: Entry,

    #[cfg(feature = "use-rtld-next")]
    pub entry: EntryCustom<()>,
}

impl fmt::Debug for Instance {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("Instance")
    }
}

fn map_queue_type(flags: vk::QueueFlags) -> queue::QueueType {
    if flags.contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE) {
        // TRANSFER_BIT optional
        queue::QueueType::General
    } else if flags.contains(vk::QueueFlags::GRAPHICS) {
        // TRANSFER_BIT optional
        queue::QueueType::Graphics
    } else if flags.contains(vk::QueueFlags::COMPUTE) {
        // TRANSFER_BIT optional
        queue::QueueType::Compute
    } else if flags.contains(vk::QueueFlags::TRANSFER) {
        queue::QueueType::Transfer
    } else {
        // TODO: present only queues?
        unimplemented!()
    }
}

unsafe fn display_debug_utils_label_ext(
    label_structs: *mut vk::DebugUtilsLabelEXT,
    count: usize,
) -> Option<String> {
    if count == 0 {
        return None;
    }

    Some(
        slice::from_raw_parts::<vk::DebugUtilsLabelEXT>(label_structs, count)
            .iter()
            .flat_map(|dul_obj| {
                dul_obj
                    .p_label_name
                    .as_ref()
                    .map(|lbl| CStr::from_ptr(lbl).to_string_lossy().into_owned())
            })
            .collect::<Vec<String>>()
            .join(", "),
    )
}

unsafe fn display_debug_utils_object_name_info_ext(
    info_structs: *mut vk::DebugUtilsObjectNameInfoEXT,
    count: usize,
) -> Option<String> {
    if count == 0 {
        return None;
    }

    //TODO: use color field of vk::DebugUtilsLabelExt in a meaningful way?
    Some(
        slice::from_raw_parts::<vk::DebugUtilsObjectNameInfoEXT>(info_structs, count)
            .iter()
            .map(|obj_info| {
                let object_name = obj_info
                    .p_object_name
                    .as_ref()
                    .map(|name| CStr::from_ptr(name).to_string_lossy().into_owned());

                match object_name {
                    Some(name) => format!(
                        "(type: {:?}, hndl: {}, name: {})",
                        obj_info.object_type,
                        &obj_info.object_handle.to_string(),
                        name
                    ),
                    None => format!(
                        "(type: {:?}, hndl: {})",
                        obj_info.object_type,
                        &obj_info.object_handle.to_string()
                    ),
                }
            })
            .collect::<Vec<String>>()
            .join(", "),
    )
}

unsafe extern "system" fn debug_utils_messenger_callback(
    message_severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    message_type: vk::DebugUtilsMessageTypeFlagsEXT,
    p_callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT,
    _user_data: *mut std::os::raw::c_void,
) -> vk::Bool32 {
    if thread::panicking() {
        return vk::FALSE;
    }
    let callback_data = *p_callback_data;

    let message_severity = match message_severity {
        vk::DebugUtilsMessageSeverityFlagsEXT::ERROR => log::Level::Error,
        vk::DebugUtilsMessageSeverityFlagsEXT::WARNING => log::Level::Warn,
        vk::DebugUtilsMessageSeverityFlagsEXT::INFO => log::Level::Info,
        vk::DebugUtilsMessageSeverityFlagsEXT::VERBOSE => log::Level::Trace,
        _ => log::Level::Warn,
    };
    let message_type = &format!("{:?}", message_type);
    let message_id_number: i32 = callback_data.message_id_number as i32;

    let message_id_name = if callback_data.p_message_id_name.is_null() {
        Cow::from("")
    } else {
        CStr::from_ptr(callback_data.p_message_id_name).to_string_lossy()
    };

    let message = if callback_data.p_message.is_null() {
        Cow::from("")
    } else {
        CStr::from_ptr(callback_data.p_message).to_string_lossy()
    };

    let additional_info: [(&str, Option<String>); 3] = [
        (
            "queue info",
            display_debug_utils_label_ext(
                callback_data.p_queue_labels as *mut _,
                callback_data.queue_label_count as usize,
            ),
        ),
        (
            "cmd buf info",
            display_debug_utils_label_ext(
                callback_data.p_cmd_buf_labels as *mut _,
                callback_data.cmd_buf_label_count as usize,
            ),
        ),
        (
            "object info",
            display_debug_utils_object_name_info_ext(
                callback_data.p_objects as *mut _,
                callback_data.object_count as usize,
            ),
        ),
    ];

    log!(message_severity, "{}\n", {
        let mut msg = format!(
            "\n{} [{} ({})] : {}",
            message_type,
            message_id_name,
            &message_id_number.to_string(),
            message
        );

        #[allow(array_into_iter)]
        for (info_label, info) in additional_info.into_iter() {
            match info {
                Some(data) => {
                    msg = format!("{}\n{}: {}", msg, info_label, data);
                }
                None => {}
            }
        }

        msg
    });

    vk::FALSE
}

unsafe extern "system" fn debug_report_callback(
    type_: vk::DebugReportFlagsEXT,
    _: vk::DebugReportObjectTypeEXT,
    _object: u64,
    _location: usize,
    _msg_code: i32,
    layer_prefix: *const std::os::raw::c_char,
    description: *const std::os::raw::c_char,
    _user_data: *mut std::os::raw::c_void,
) -> vk::Bool32 {
    if thread::panicking() {
        return vk::FALSE;
    }

    let level = match type_ {
        vk::DebugReportFlagsEXT::ERROR => log::Level::Error,
        vk::DebugReportFlagsEXT::WARNING => log::Level::Warn,
        vk::DebugReportFlagsEXT::INFORMATION => log::Level::Info,
        vk::DebugReportFlagsEXT::DEBUG => log::Level::Debug,
        _ => log::Level::Warn,
    };

    let layer_prefix = CStr::from_ptr(layer_prefix).to_str().unwrap();
    let description = CStr::from_ptr(description).to_str().unwrap();
    log!(level, "[{}] {}", layer_prefix, description);
    vk::FALSE
}

impl hal::Instance<Backend> for Instance {
    fn create(name: &str, version: u32) -> Result<Self, hal::UnsupportedBackend> {
        #[cfg(not(feature = "use-rtld-next"))]
        let entry = match Entry::new() {
            Ok(entry) => entry,
            Err(err) => {
                info!("Missing Vulkan entry points: {:?}", err);
                return Err(hal::UnsupportedBackend);
            }
        };

        #[cfg(feature = "use-rtld-next")]
        let entry = EntryCustom::new_custom((), |_, name| unsafe {
            DynamicLibrary::symbol_special(SpecialHandles::Next, &*name.to_string_lossy())
                .unwrap_or(std::ptr::null_mut())
        });

        let app_name = CString::new(name).unwrap();
        let app_info = vk::ApplicationInfo::builder()
            .application_name(app_name.as_c_str())
            .application_version(version)
            .engine_name(CStr::from_bytes_with_nul(b"gfx-rs\0").unwrap())
            .engine_version(1)
            .api_version(vk::make_version(1, 0, 0));

        let instance_extensions = entry
            .enumerate_instance_extension_properties()
            .map_err(|e| {
                info!("Unable to enumerate instance extensions: {:?}", e);
                hal::UnsupportedBackend
            })?;

        let instance_layers = entry.enumerate_instance_layer_properties().map_err(|e| {
            info!("Unable to enumerate instance layers: {:?}", e);
            hal::UnsupportedBackend
        })?;

        // Check our extensions against the available extensions
        let extensions = {
            let mut extensions: Vec<&'static CStr> = Vec::new();
            extensions.push(extensions::khr::Surface::name());

            // Platform-specific WSI extensions
            if cfg!(all(
                unix,
                not(target_os = "android"),
                not(target_os = "macos")
            )) {
                extensions.push(extensions::khr::XlibSurface::name());
                extensions.push(extensions::khr::XcbSurface::name());
                extensions.push(extensions::khr::WaylandSurface::name());
            }
            if cfg!(target_os = "android") {
                extensions.push(extensions::khr::AndroidSurface::name());
            }
            if cfg!(target_os = "windows") {
                extensions.push(extensions::khr::Win32Surface::name());
            }
            if cfg!(target_os = "macos") {
                extensions.push(extensions::mvk::MacOSSurface::name());
            }

            extensions.push(DebugUtils::name());
            if cfg!(debug_assertions) {
                extensions.push(DebugReport::name());
            }

            extensions.push(vk::KhrGetPhysicalDeviceProperties2Fn::name());

            // Only keep available extensions.
            extensions.retain(|&ext| {
                if instance_extensions
                    .iter()
                    .find(|inst_ext| unsafe {
                        CStr::from_ptr(inst_ext.extension_name.as_ptr()) == ext
                    })
                    .is_some()
                {
                    true
                } else {
                    info!("Unable to find extension: {}", ext.to_string_lossy());
                    false
                }
            });
            extensions
        };

        // Check requested layers against the available layers
        let layers = {
            let mut layers: Vec<&'static CStr> = Vec::new();
            if cfg!(debug_assertions) {
                layers.push(CStr::from_bytes_with_nul(b"VK_LAYER_KHRONOS_validation\0").unwrap());
            }

            // Only keep available layers.
            layers.retain(|&layer| {
                if instance_layers
                    .iter()
                    .find(|inst_layer| unsafe {
                        CStr::from_ptr(inst_layer.layer_name.as_ptr()) == layer
                    })
                    .is_some()
                {
                    true
                } else {
                    warn!("Unable to find layer: {}", layer.to_string_lossy());
                    false
                }
            });
            layers
        };

        let instance = {
            let str_pointers = layers
                .iter()
                .chain(extensions.iter())
                .map(|&s| {
                    // Safe because `layers` and `extensions` entries have static lifetime.
                    s.as_ptr()
                })
                .collect::<Vec<_>>();

            let create_info = vk::InstanceCreateInfo::builder()
                .flags(vk::InstanceCreateFlags::empty())
                .application_info(&app_info)
                .enabled_layer_names(&str_pointers[..layers.len()])
                .enabled_extension_names(&str_pointers[layers.len()..]);

            unsafe { entry.create_instance(&create_info, None) }.map_err(|e| {
                warn!("Unable to create Vulkan instance: {:?}", e);
                hal::UnsupportedBackend
            })?
        };

        let get_physical_device_properties = extensions
            .iter()
            .find(|&&ext| ext == vk::KhrGetPhysicalDeviceProperties2Fn::name())
            .map(|_| {
                vk::KhrGetPhysicalDeviceProperties2Fn::load(|name| unsafe {
                    std::mem::transmute(
                        entry.get_instance_proc_addr(instance.handle(), name.as_ptr()),
                    )
                })
            });

        let debug_messenger = {
            // make sure VK_EXT_debug_utils is available
            if instance_extensions.iter().any(|props| unsafe {
                CStr::from_ptr(props.extension_name.as_ptr()) == DebugUtils::name()
            }) {
                let ext = DebugUtils::new(&entry, &instance);
                let info = vk::DebugUtilsMessengerCreateInfoEXT::builder()
                    .flags(vk::DebugUtilsMessengerCreateFlagsEXT::empty())
                    .message_severity(vk::DebugUtilsMessageSeverityFlagsEXT::all())
                    .message_type(vk::DebugUtilsMessageTypeFlagsEXT::all())
                    .pfn_user_callback(Some(debug_utils_messenger_callback));
                let handle = unsafe { ext.create_debug_utils_messenger(&info, None) }.unwrap();
                Some(DebugMessenger::Utils(ext, handle))
            } else if cfg!(debug_assertions)
                && instance_extensions.iter().any(|props| unsafe {
                    CStr::from_ptr(props.extension_name.as_ptr()) == DebugReport::name()
                })
            {
                let ext = DebugReport::new(&entry, &instance);
                let info = vk::DebugReportCallbackCreateInfoEXT::builder()
                    .flags(vk::DebugReportFlagsEXT::all())
                    .pfn_callback(Some(debug_report_callback));
                let handle = unsafe { ext.create_debug_report_callback(&info, None) }.unwrap();
                Some(DebugMessenger::Report(ext, handle))
            } else {
                None
            }
        };

        Ok(Instance {
            raw: Arc::new(RawInstance {
                inner: instance,
                debug_messenger,
                get_physical_device_properties,
            }),
            extensions,
            entry,
        })
    }

    fn enumerate_adapters(&self) -> Vec<adapter::Adapter<Backend>> {
        let devices = match unsafe { self.raw.inner.enumerate_physical_devices() } {
            Ok(devices) => devices,
            Err(err) => {
                error!("Could not enumerate physical devices! {}", err);
                vec![]
            }
        };

        devices
            .into_iter()
            .map(|device| {
                let extensions =
                    unsafe { self.raw.inner.enumerate_device_extension_properties(device) }
                        .unwrap();
                let properties = unsafe { self.raw.inner.get_physical_device_properties(device) };
                let info = adapter::AdapterInfo {
                    name: unsafe {
                        CStr::from_ptr(properties.device_name.as_ptr())
                            .to_str()
                            .unwrap_or("Unknown")
                            .to_owned()
                    },
                    vendor: properties.vendor_id as usize,
                    device: properties.device_id as usize,
                    device_type: match properties.device_type {
                        ash::vk::PhysicalDeviceType::OTHER => adapter::DeviceType::Other,
                        ash::vk::PhysicalDeviceType::INTEGRATED_GPU => {
                            adapter::DeviceType::IntegratedGpu
                        }
                        ash::vk::PhysicalDeviceType::DISCRETE_GPU => {
                            adapter::DeviceType::DiscreteGpu
                        }
                        ash::vk::PhysicalDeviceType::VIRTUAL_GPU => adapter::DeviceType::VirtualGpu,
                        ash::vk::PhysicalDeviceType::CPU => adapter::DeviceType::Cpu,
                        _ => adapter::DeviceType::Other,
                    },
                };
                let physical_device = PhysicalDevice {
                    instance: self.raw.clone(),
                    handle: device,
                    extensions,
                    properties,
                    known_memory_flags: vk::MemoryPropertyFlags::DEVICE_LOCAL
                        | vk::MemoryPropertyFlags::HOST_VISIBLE
                        | vk::MemoryPropertyFlags::HOST_COHERENT
                        | vk::MemoryPropertyFlags::HOST_CACHED
                        | vk::MemoryPropertyFlags::LAZILY_ALLOCATED,
                };
                let queue_families = unsafe {
                    self.raw
                        .inner
                        .get_physical_device_queue_family_properties(device)
                        .into_iter()
                        .enumerate()
                        .map(|(i, properties)| QueueFamily {
                            properties,
                            device,
                            index: i as u32,
                        })
                        .collect()
                };

                adapter::Adapter {
                    info,
                    physical_device,
                    queue_families,
                }
            })
            .collect()
    }

    unsafe fn create_surface(
        &self,
        has_handle: &impl raw_window_handle::HasRawWindowHandle,
    ) -> Result<window::Surface, hal::window::InitError> {
        use raw_window_handle::RawWindowHandle;

        match has_handle.raw_window_handle() {
            #[cfg(all(
                unix,
                not(target_os = "android"),
                not(target_os = "macos"),
                not(target_os = "solaris")
            ))]
            RawWindowHandle::Wayland(handle)
                if self
                    .extensions
                    .contains(&extensions::khr::WaylandSurface::name()) =>
            {
                Ok(self.create_surface_from_wayland(handle.display, handle.surface))
            }
            #[cfg(all(
                unix,
                not(target_os = "android"),
                not(target_os = "macos"),
                not(target_os = "solaris")
            ))]
            RawWindowHandle::Xlib(handle)
                if self
                    .extensions
                    .contains(&extensions::khr::XlibSurface::name()) =>
            {
                Ok(self.create_surface_from_xlib(handle.display as *mut _, handle.window))
            }
            #[cfg(all(
                unix,
                not(target_os = "android"),
                not(target_os = "macos"),
                not(target_os = "ios")
            ))]
            RawWindowHandle::Xcb(handle)
                if self
                    .extensions
                    .contains(&extensions::khr::XcbSurface::name()) =>
            {
                Ok(self.create_surface_from_xcb(handle.connection as *mut _, handle.window))
            }
            #[cfg(target_os = "android")]
            RawWindowHandle::Android(handle) => {
                Ok(self.create_surface_android(handle.a_native_window))
            }
            #[cfg(windows)]
            RawWindowHandle::Windows(handle) => {
                use winapi::um::libloaderapi::GetModuleHandleW;

                let hinstance = GetModuleHandleW(std::ptr::null());
                Ok(self.create_surface_from_hwnd(hinstance as *mut _, handle.hwnd))
            }
            #[cfg(target_os = "macos")]
            RawWindowHandle::MacOS(handle) => Ok(self.create_surface_from_ns_view(handle.ns_view)),
            _ => Err(hal::window::InitError::UnsupportedWindowHandle),
        }
    }

    unsafe fn destroy_surface(&self, surface: window::Surface) {
        surface
            .raw
            .functor
            .destroy_surface(surface.raw.handle, None);
    }
}

#[derive(Debug, Clone)]
pub struct QueueFamily {
    properties: vk::QueueFamilyProperties,
    device: vk::PhysicalDevice,
    index: u32,
}

impl queue::QueueFamily for QueueFamily {
    fn queue_type(&self) -> queue::QueueType {
        map_queue_type(self.properties.queue_flags)
    }
    fn max_queues(&self) -> usize {
        self.properties.queue_count as _
    }
    fn id(&self) -> queue::QueueFamilyId {
        queue::QueueFamilyId(self.index as _)
    }
}

pub struct PhysicalDevice {
    instance: Arc<RawInstance>,
    handle: vk::PhysicalDevice,
    extensions: Vec<vk::ExtensionProperties>,
    properties: vk::PhysicalDeviceProperties,
    known_memory_flags: vk::MemoryPropertyFlags,
}

impl PhysicalDevice {
    fn supports_extension(&self, extension: &CStr) -> bool {
        self.extensions
            .iter()
            .any(|ep| unsafe { CStr::from_ptr(ep.extension_name.as_ptr()) } == extension)
    }
}

impl fmt::Debug for PhysicalDevice {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("PhysicalDevice")
    }
}

pub struct DeviceCreationFeatures {
    core: vk::PhysicalDeviceFeatures,
    descriptor_indexing: Option<vk::PhysicalDeviceDescriptorIndexingFeaturesEXT>,
    mesh_shaders: Option<vk::PhysicalDeviceMeshShaderFeaturesNV>,
    imageless_framebuffers: Option<vk::PhysicalDeviceImagelessFramebufferFeaturesKHR>,
}

impl adapter::PhysicalDevice<Backend> for PhysicalDevice {
    unsafe fn open(
        &self,
        families: &[(&QueueFamily, &[queue::QueuePriority])],
        requested_features: Features,
    ) -> Result<adapter::Gpu<Backend>, DeviceCreationError> {
        let family_infos = families
            .iter()
            .map(|&(family, priorities)| {
                vk::DeviceQueueCreateInfo::builder()
                    .flags(vk::DeviceQueueCreateFlags::empty())
                    .queue_family_index(family.index)
                    .queue_priorities(priorities)
                    .build()
            })
            .collect::<Vec<_>>();

        if !self.features().contains(requested_features) {
            return Err(DeviceCreationError::MissingFeature);
        }

        let imageless_framebuffers = self.supports_extension(vk::KhrImagelessFramebufferFn::name());
        let maintenance_level = if imageless_framebuffers {
            2
        } else if self.supports_extension(vk::KhrMaintenance1Fn::name()) {
            1
        } else {
            0
        };

        let mut enabled_features =
            conv::map_device_features(requested_features, imageless_framebuffers);
        let enabled_extensions = {
            let mut enabled_extensions: Vec<&'static CStr> = Vec::new();
            enabled_extensions.push(extensions::khr::Swapchain::name());

            match maintenance_level {
                0 => {
                    if requested_features.contains(Features::NDC_Y_UP) {
                        enabled_extensions.push(vk::AmdNegativeViewportHeightFn::name());
                    }
                }
                1 => {
                    enabled_extensions.push(vk::KhrMaintenance1Fn::name());
                }
                2 => {
                    enabled_extensions.push(vk::KhrMaintenance2Fn::name());
                }
                _ => unreachable!(),
            }

            if imageless_framebuffers {
                enabled_extensions.push(vk::KhrImageFormatListFn::name());
                enabled_extensions.push(vk::KhrImagelessFramebufferFn::name());
            }

            if requested_features.intersects(
                Features::SAMPLED_TEXTURE_DESCRIPTOR_INDEXING
                    | Features::STORAGE_TEXTURE_DESCRIPTOR_INDEXING
                    | Features::UNSIZED_DESCRIPTOR_ARRAY,
            ) {
                enabled_extensions.push(vk::KhrMaintenance3Fn::name());
                enabled_extensions.push(vk::ExtDescriptorIndexingFn::name());
            }

            if requested_features.intersects(Features::TASK_SHADER | Features::MESH_SHADER) {
                enabled_extensions.push(MeshShader::name());
            }

            if requested_features.contains(Features::DRAW_INDIRECT_COUNT) {
                enabled_extensions.push(DrawIndirectCount::name());
            }

            enabled_extensions
        };

        let valid_ash_memory_types = {
            let mem_properties = self
                .instance
                .inner
                .get_physical_device_memory_properties(self.handle);
            mem_properties.memory_types[..mem_properties.memory_type_count as usize]
                .iter()
                .enumerate()
                .fold(0, |u, (i, mem)| {
                    if self.known_memory_flags.contains(mem.property_flags) {
                        u | (1 << i)
                    } else {
                        u
                    }
                })
        };

        // Create device
        let device_raw = {
            let str_pointers = enabled_extensions
                .iter()
                .map(|&s| {
                    // Safe because `enabled_extensions` entries have static lifetime.
                    s.as_ptr()
                })
                .collect::<Vec<_>>();

            let mut info = vk::DeviceCreateInfo::builder()
                .queue_create_infos(&family_infos)
                .enabled_extension_names(&str_pointers)
                .enabled_features(&enabled_features.core);
            if let Some(ref mut feature) = enabled_features.descriptor_indexing {
                info = info.push_next(feature);
            }
            if let Some(ref mut feature) = enabled_features.mesh_shaders {
                info = info.push_next(feature);
            }
            if let Some(ref mut feature) = enabled_features.imageless_framebuffers {
                info = info.push_next(feature);
            }

            match self.instance.inner.create_device(self.handle, &info, None) {
                Ok(device) => device,
                Err(e) => {
                    return Err(match e {
                        vk::Result::ERROR_OUT_OF_HOST_MEMORY => {
                            DeviceCreationError::OutOfMemory(OutOfMemory::Host)
                        }
                        vk::Result::ERROR_OUT_OF_DEVICE_MEMORY => {
                            DeviceCreationError::OutOfMemory(OutOfMemory::Device)
                        }
                        vk::Result::ERROR_INITIALIZATION_FAILED => {
                            DeviceCreationError::InitializationFailed
                        }
                        vk::Result::ERROR_DEVICE_LOST => DeviceCreationError::DeviceLost,
                        vk::Result::ERROR_TOO_MANY_OBJECTS => DeviceCreationError::TooManyObjects,
                        _ => unreachable!(),
                    })
                }
            }
        };

        let swapchain_fn = Swapchain::new(&self.instance.inner, &device_raw);

        let mesh_fn =
            if requested_features.intersects(Features::TASK_SHADER | Features::MESH_SHADER) {
                Some(MeshShader::new(&self.instance.inner, &device_raw))
            } else {
                None
            };

        let indirect_count_fn = if requested_features.contains(Features::DRAW_INDIRECT_COUNT) {
            Some(DrawIndirectCount::new(&self.instance.inner, &device_raw))
        } else {
            None
        };

        let device = Device {
            shared: Arc::new(RawDevice {
                raw: device_raw,
                features: requested_features,
                instance: Arc::clone(&self.instance),
                extension_fns: DeviceExtensionFunctions {
                    mesh_shaders: mesh_fn,
                    draw_indirect_count: indirect_count_fn,
                },
                maintenance_level,
                imageless_framebuffers,
            }),
            vendor_id: self.properties.vendor_id,
            valid_ash_memory_types,
        };

        let device_arc = Arc::clone(&device.shared);
        let queue_groups = families
            .into_iter()
            .map(|&(family, ref priorities)| {
                let mut family_raw =
                    queue::QueueGroup::new(queue::QueueFamilyId(family.index as usize));
                for id in 0..priorities.len() {
                    let queue_raw = device_arc.raw.get_device_queue(family.index, id as _);
                    family_raw.add_queue(CommandQueue {
                        raw: Arc::new(queue_raw),
                        device: device_arc.clone(),
                        swapchain_fn: swapchain_fn.clone(),
                    });
                }
                family_raw
            })
            .collect();

        Ok(adapter::Gpu {
            device,
            queue_groups,
        })
    }

    fn format_properties(&self, format: Option<format::Format>) -> format::Properties {
        let properties = unsafe {
            self.instance.inner.get_physical_device_format_properties(
                self.handle,
                format.map_or(vk::Format::UNDEFINED, conv::map_format),
            )
        };

        format::Properties {
            linear_tiling: conv::map_image_features(properties.linear_tiling_features),
            optimal_tiling: conv::map_image_features(properties.optimal_tiling_features),
            buffer_features: conv::map_buffer_features(properties.buffer_features),
        }
    }

    fn image_format_properties(
        &self,
        format: format::Format,
        dimensions: u8,
        tiling: image::Tiling,
        usage: image::Usage,
        view_caps: image::ViewCapabilities,
    ) -> Option<image::FormatProperties> {
        let format_properties = unsafe {
            self.instance
                .inner
                .get_physical_device_image_format_properties(
                    self.handle,
                    conv::map_format(format),
                    match dimensions {
                        1 => vk::ImageType::TYPE_1D,
                        2 => vk::ImageType::TYPE_2D,
                        3 => vk::ImageType::TYPE_3D,
                        _ => panic!("Unexpected image dimensionality: {}", dimensions),
                    },
                    conv::map_tiling(tiling),
                    conv::map_image_usage(usage),
                    conv::map_view_capabilities(view_caps),
                )
        };

        match format_properties {
            Ok(props) => Some(image::FormatProperties {
                max_extent: image::Extent {
                    width: props.max_extent.width,
                    height: props.max_extent.height,
                    depth: props.max_extent.depth,
                },
                max_levels: props.max_mip_levels as _,
                max_layers: props.max_array_layers as _,
                sample_count_mask: props.sample_counts.as_raw() as _,
                max_resource_size: props.max_resource_size as _,
            }),
            Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED) => None,
            Err(other) => {
                error!("Unexpected error in `image_format_properties`: {:?}", other);
                None
            }
        }
    }

    fn memory_properties(&self) -> adapter::MemoryProperties {
        let mem_properties = unsafe {
            self.instance
                .inner
                .get_physical_device_memory_properties(self.handle)
        };
        let memory_heaps = mem_properties.memory_heaps[..mem_properties.memory_heap_count as usize]
            .iter()
            .map(|mem| adapter::MemoryHeap {
                size: mem.size,
                flags: conv::map_vk_memory_heap_flags(mem.flags),
            })
            .collect();
        let memory_types = mem_properties.memory_types[..mem_properties.memory_type_count as usize]
            .iter()
            .filter_map(|mem| {
                if self.known_memory_flags.contains(mem.property_flags) {
                    Some(adapter::MemoryType {
                        properties: conv::map_vk_memory_properties(mem.property_flags),
                        heap_index: mem.heap_index as usize,
                    })
                } else {
                    warn!(
                        "Skipping memory type with unknown flags {:?}",
                        mem.property_flags
                    );
                    None
                }
            })
            .collect();

        adapter::MemoryProperties {
            memory_heaps,
            memory_types,
        }
    }

    fn features(&self) -> Features {
        // see https://github.com/gfx-rs/gfx/issues/1930
        let is_windows_intel_dual_src_bug = cfg!(windows)
            && self.properties.vendor_id == info::intel::VENDOR
            && (self.properties.device_id & info::intel::DEVICE_KABY_LAKE_MASK
                == info::intel::DEVICE_KABY_LAKE_MASK
                || self.properties.device_id & info::intel::DEVICE_SKY_LAKE_MASK
                    == info::intel::DEVICE_SKY_LAKE_MASK);

        let mut descriptor_indexing_features = None;
        let features = if let Some(ref get_device_properties) =
            self.instance.get_physical_device_properties
        {
            let features = vk::PhysicalDeviceFeatures::builder().build();
            let mut features2 = vk::PhysicalDeviceFeatures2KHR::builder()
                .features(features)
                .build();

            // Add extension infos to the p_next chain
            if self.supports_extension(vk::ExtDescriptorIndexingFn::name()) {
                descriptor_indexing_features =
                    Some(vk::PhysicalDeviceDescriptorIndexingFeaturesEXT::builder().build());

                let mut_ref = descriptor_indexing_features.as_mut().unwrap();
                mut_ref.p_next = mem::replace(&mut features2.p_next, mut_ref as *mut _ as *mut _);
            }

            unsafe {
                get_device_properties
                    .get_physical_device_features2_khr(self.handle, &mut features2 as *mut _);
            }
            features2.features
        } else {
            unsafe {
                self.instance
                    .inner
                    .get_physical_device_features(self.handle)
            }
        };

        let mut bits = Features::empty()
            | Features::TRIANGLE_FAN
            | Features::SEPARATE_STENCIL_REF_VALUES
            | Features::SAMPLER_MIP_LOD_BIAS
            | Features::SAMPLER_BORDER_COLOR
            | Features::MUTABLE_COMPARISON_SAMPLER
            | Features::TEXTURE_DESCRIPTOR_ARRAY;

        if self.supports_extension(vk::AmdNegativeViewportHeightFn::name())
            || self.supports_extension(vk::KhrMaintenance1Fn::name())
        {
            bits |= Features::NDC_Y_UP;
        }
        if self.supports_extension(vk::KhrSamplerMirrorClampToEdgeFn::name()) {
            bits |= Features::SAMPLER_MIRROR_CLAMP_EDGE;
        }
        if self.supports_extension(DrawIndirectCount::name()) {
            bits |= Features::DRAW_INDIRECT_COUNT
        }
        // This will only be some if the extension exists
        if let Some(ref desc_indexing) = descriptor_indexing_features {
            if desc_indexing.shader_sampled_image_array_non_uniform_indexing != 0 {
                bits |= Features::SAMPLED_TEXTURE_DESCRIPTOR_INDEXING;
            }
            if desc_indexing.shader_storage_image_array_non_uniform_indexing != 0 {
                bits |= Features::STORAGE_TEXTURE_DESCRIPTOR_INDEXING;
            }
            if desc_indexing.runtime_descriptor_array != 0 {
                bits |= Features::UNSIZED_DESCRIPTOR_ARRAY;
            }
        }

        if features.robust_buffer_access != 0 {
            bits |= Features::ROBUST_BUFFER_ACCESS;
        }
        if features.full_draw_index_uint32 != 0 {
            bits |= Features::FULL_DRAW_INDEX_U32;
        }
        if features.image_cube_array != 0 {
            bits |= Features::IMAGE_CUBE_ARRAY;
        }
        if features.independent_blend != 0 {
            bits |= Features::INDEPENDENT_BLENDING;
        }
        if features.geometry_shader != 0 {
            bits |= Features::GEOMETRY_SHADER;
        }
        if features.tessellation_shader != 0 {
            bits |= Features::TESSELLATION_SHADER;
        }
        if features.sample_rate_shading != 0 {
            bits |= Features::SAMPLE_RATE_SHADING;
        }
        if features.dual_src_blend != 0 && !is_windows_intel_dual_src_bug {
            bits |= Features::DUAL_SRC_BLENDING;
        }
        if features.logic_op != 0 {
            bits |= Features::LOGIC_OP;
        }
        if features.multi_draw_indirect != 0 {
            bits |= Features::MULTI_DRAW_INDIRECT;
        }
        if features.draw_indirect_first_instance != 0 {
            bits |= Features::DRAW_INDIRECT_FIRST_INSTANCE;
        }
        if features.depth_clamp != 0 {
            bits |= Features::DEPTH_CLAMP;
        }
        if features.depth_bias_clamp != 0 {
            bits |= Features::DEPTH_BIAS_CLAMP;
        }
        if features.fill_mode_non_solid != 0 {
            bits |= Features::NON_FILL_POLYGON_MODE;
        }
        if features.depth_bounds != 0 {
            bits |= Features::DEPTH_BOUNDS;
        }
        if features.wide_lines != 0 {
            bits |= Features::LINE_WIDTH;
        }
        if features.large_points != 0 {
            bits |= Features::POINT_SIZE;
        }
        if features.alpha_to_one != 0 {
            bits |= Features::ALPHA_TO_ONE;
        }
        if features.multi_viewport != 0 {
            bits |= Features::MULTI_VIEWPORTS;
        }
        if features.sampler_anisotropy != 0 {
            bits |= Features::SAMPLER_ANISOTROPY;
        }
        if features.texture_compression_etc2 != 0 {
            bits |= Features::FORMAT_ETC2;
        }
        if features.texture_compression_astc_ldr != 0 {
            bits |= Features::FORMAT_ASTC_LDR;
        }
        if features.texture_compression_bc != 0 {
            bits |= Features::FORMAT_BC;
        }
        if features.occlusion_query_precise != 0 {
            bits |= Features::PRECISE_OCCLUSION_QUERY;
        }
        if features.pipeline_statistics_query != 0 {
            bits |= Features::PIPELINE_STATISTICS_QUERY;
        }
        if features.vertex_pipeline_stores_and_atomics != 0 {
            bits |= Features::VERTEX_STORES_AND_ATOMICS;
        }
        if features.fragment_stores_and_atomics != 0 {
            bits |= Features::FRAGMENT_STORES_AND_ATOMICS;
        }
        if features.shader_tessellation_and_geometry_point_size != 0 {
            bits |= Features::SHADER_TESSELLATION_AND_GEOMETRY_POINT_SIZE;
        }
        if features.shader_image_gather_extended != 0 {
            bits |= Features::SHADER_IMAGE_GATHER_EXTENDED;
        }
        if features.shader_storage_image_extended_formats != 0 {
            bits |= Features::SHADER_STORAGE_IMAGE_EXTENDED_FORMATS;
        }
        if features.shader_storage_image_multisample != 0 {
            bits |= Features::SHADER_STORAGE_IMAGE_MULTISAMPLE;
        }
        if features.shader_storage_image_read_without_format != 0 {
            bits |= Features::SHADER_STORAGE_IMAGE_READ_WITHOUT_FORMAT;
        }
        if features.shader_storage_image_write_without_format != 0 {
            bits |= Features::SHADER_STORAGE_IMAGE_WRITE_WITHOUT_FORMAT;
        }
        if features.shader_uniform_buffer_array_dynamic_indexing != 0 {
            bits |= Features::SHADER_UNIFORM_BUFFER_ARRAY_DYNAMIC_INDEXING;
        }
        if features.shader_sampled_image_array_dynamic_indexing != 0 {
            bits |= Features::SHADER_SAMPLED_IMAGE_ARRAY_DYNAMIC_INDEXING;
        }
        if features.shader_storage_buffer_array_dynamic_indexing != 0 {
            bits |= Features::SHADER_STORAGE_BUFFER_ARRAY_DYNAMIC_INDEXING;
        }
        if features.shader_storage_image_array_dynamic_indexing != 0 {
            bits |= Features::SHADER_STORAGE_IMAGE_ARRAY_DYNAMIC_INDEXING;
        }
        if features.shader_clip_distance != 0 {
            bits |= Features::SHADER_CLIP_DISTANCE;
        }
        if features.shader_cull_distance != 0 {
            bits |= Features::SHADER_CULL_DISTANCE;
        }
        if features.shader_float64 != 0 {
            bits |= Features::SHADER_FLOAT64;
        }
        if features.shader_int64 != 0 {
            bits |= Features::SHADER_INT64;
        }
        if features.shader_int16 != 0 {
            bits |= Features::SHADER_INT16;
        }
        if features.shader_resource_residency != 0 {
            bits |= Features::SHADER_RESOURCE_RESIDENCY;
        }
        if features.shader_resource_min_lod != 0 {
            bits |= Features::SHADER_RESOURCE_MIN_LOD;
        }
        if features.sparse_binding != 0 {
            bits |= Features::SPARSE_BINDING;
        }
        if features.sparse_residency_buffer != 0 {
            bits |= Features::SPARSE_RESIDENCY_BUFFER;
        }
        if features.sparse_residency_image2_d != 0 {
            bits |= Features::SPARSE_RESIDENCY_IMAGE_2D;
        }
        if features.sparse_residency_image3_d != 0 {
            bits |= Features::SPARSE_RESIDENCY_IMAGE_3D;
        }
        if features.sparse_residency2_samples != 0 {
            bits |= Features::SPARSE_RESIDENCY_2_SAMPLES;
        }
        if features.sparse_residency4_samples != 0 {
            bits |= Features::SPARSE_RESIDENCY_4_SAMPLES;
        }
        if features.sparse_residency8_samples != 0 {
            bits |= Features::SPARSE_RESIDENCY_8_SAMPLES;
        }
        if features.sparse_residency16_samples != 0 {
            bits |= Features::SPARSE_RESIDENCY_16_SAMPLES;
        }
        if features.sparse_residency_aliased != 0 {
            bits |= Features::SPARSE_RESIDENCY_ALIASED;
        }
        if features.variable_multisample_rate != 0 {
            bits |= Features::VARIABLE_MULTISAMPLE_RATE;
        }
        if features.inherited_queries != 0 {
            bits |= Features::INHERITED_QUERIES;
        }
        if self.supports_extension(MeshShader::name()) {
            bits |= Features::TASK_SHADER;
            bits |= Features::MESH_SHADER
        }

        bits
    }

    fn hints(&self) -> Hints {
        Hints::BASE_VERTEX_INSTANCE_DRAWING
    }

    fn limits(&self) -> Limits {
        let limits = &self.properties.limits;
        let max_group_count = limits.max_compute_work_group_count;
        let max_group_size = limits.max_compute_work_group_size;

        Limits {
            max_image_1d_size: limits.max_image_dimension1_d,
            max_image_2d_size: limits.max_image_dimension2_d,
            max_image_3d_size: limits.max_image_dimension3_d,
            max_image_cube_size: limits.max_image_dimension_cube,
            max_image_array_layers: limits.max_image_array_layers as _,
            max_texel_elements: limits.max_texel_buffer_elements as _,
            max_patch_size: limits.max_tessellation_patch_size as PatchSize,
            max_viewports: limits.max_viewports as _,
            max_viewport_dimensions: limits.max_viewport_dimensions,
            max_framebuffer_extent: image::Extent {
                width: limits.max_framebuffer_width,
                height: limits.max_framebuffer_height,
                depth: limits.max_framebuffer_layers,
            },
            max_compute_work_group_count: [
                max_group_count[0] as _,
                max_group_count[1] as _,
                max_group_count[2] as _,
            ],
            max_compute_work_group_size: [
                max_group_size[0] as _,
                max_group_size[1] as _,
                max_group_size[2] as _,
            ],
            max_vertex_input_attributes: limits.max_vertex_input_attributes as _,
            max_vertex_input_bindings: limits.max_vertex_input_bindings as _,
            max_vertex_input_attribute_offset: limits.max_vertex_input_attribute_offset as _,
            max_vertex_input_binding_stride: limits.max_vertex_input_binding_stride as _,
            max_vertex_output_components: limits.max_vertex_output_components as _,
            optimal_buffer_copy_offset_alignment: limits.optimal_buffer_copy_offset_alignment as _,
            optimal_buffer_copy_pitch_alignment: limits.optimal_buffer_copy_row_pitch_alignment
                as _,
            min_texel_buffer_offset_alignment: limits.min_texel_buffer_offset_alignment as _,
            min_uniform_buffer_offset_alignment: limits.min_uniform_buffer_offset_alignment as _,
            min_storage_buffer_offset_alignment: limits.min_storage_buffer_offset_alignment as _,
            framebuffer_color_sample_counts: limits.framebuffer_color_sample_counts.as_raw() as _,
            framebuffer_depth_sample_counts: limits.framebuffer_depth_sample_counts.as_raw() as _,
            framebuffer_stencil_sample_counts: limits.framebuffer_stencil_sample_counts.as_raw()
                as _,
            timestamp_compute_and_graphics: limits.timestamp_compute_and_graphics != 0,
            timestamp_period: limits.timestamp_period,
            max_color_attachments: limits.max_color_attachments as _,
            buffer_image_granularity: limits.buffer_image_granularity,
            non_coherent_atom_size: limits.non_coherent_atom_size as _,
            max_sampler_anisotropy: limits.max_sampler_anisotropy,
            min_vertex_input_binding_stride_alignment: 1,
            max_bound_descriptor_sets: limits.max_bound_descriptor_sets as _,
            max_compute_shared_memory_size: limits.max_compute_shared_memory_size as _,
            max_compute_work_group_invocations: limits.max_compute_work_group_invocations as _,
            max_descriptor_set_input_attachments: limits.max_descriptor_set_input_attachments as _,
            max_descriptor_set_sampled_images: limits.max_descriptor_set_sampled_images as _,
            max_descriptor_set_samplers: limits.max_descriptor_set_samplers as _,
            max_descriptor_set_storage_buffers: limits.max_descriptor_set_storage_buffers as _,
            max_descriptor_set_storage_buffers_dynamic: limits
                .max_descriptor_set_storage_buffers_dynamic
                as _,
            max_descriptor_set_storage_images: limits.max_descriptor_set_storage_images as _,
            max_descriptor_set_uniform_buffers: limits.max_descriptor_set_uniform_buffers as _,
            max_descriptor_set_uniform_buffers_dynamic: limits
                .max_descriptor_set_uniform_buffers_dynamic
                as _,
            max_draw_indexed_index_value: limits.max_draw_indexed_index_value,
            max_draw_indirect_count: limits.max_draw_indirect_count,
            max_fragment_combined_output_resources: limits.max_fragment_combined_output_resources
                as _,
            max_fragment_dual_source_attachments: limits.max_fragment_dual_src_attachments as _,
            max_fragment_input_components: limits.max_fragment_input_components as _,
            max_fragment_output_attachments: limits.max_fragment_output_attachments as _,
            max_framebuffer_layers: limits.max_framebuffer_layers as _,
            max_geometry_input_components: limits.max_geometry_input_components as _,
            max_geometry_output_components: limits.max_geometry_output_components as _,
            max_geometry_output_vertices: limits.max_geometry_output_vertices as _,
            max_geometry_shader_invocations: limits.max_geometry_shader_invocations as _,
            max_geometry_total_output_components: limits.max_geometry_total_output_components as _,
            max_memory_allocation_count: limits.max_memory_allocation_count as _,
            max_per_stage_descriptor_input_attachments: limits
                .max_per_stage_descriptor_input_attachments
                as _,
            max_per_stage_descriptor_sampled_images: limits.max_per_stage_descriptor_sampled_images
                as _,
            max_per_stage_descriptor_samplers: limits.max_per_stage_descriptor_samplers as _,
            max_per_stage_descriptor_storage_buffers: limits
                .max_per_stage_descriptor_storage_buffers
                as _,
            max_per_stage_descriptor_storage_images: limits.max_per_stage_descriptor_storage_images
                as _,
            max_per_stage_descriptor_uniform_buffers: limits
                .max_per_stage_descriptor_uniform_buffers
                as _,
            max_per_stage_resources: limits.max_per_stage_resources as _,
            max_push_constants_size: limits.max_push_constants_size as _,
            max_sampler_allocation_count: limits.max_sampler_allocation_count as _,
            max_sampler_lod_bias: limits.max_sampler_lod_bias as _,
            max_storage_buffer_range: limits.max_storage_buffer_range as _,
            max_uniform_buffer_range: limits.max_uniform_buffer_range as _,
            min_memory_map_alignment: limits.min_memory_map_alignment,
            standard_sample_locations: limits.standard_sample_locations == ash::vk::TRUE,

            // TODO: Implement Limits for Mesh Shaders
            //       Depends on VkPhysicalDeviceMeshShaderPropertiesNV which depends on VkPhysicalProperties2
            max_draw_mesh_tasks_count: 0,
            max_task_work_group_invocations: 0,
            max_task_work_group_size: [0; 3],
            max_task_total_memory_size: 0,
            max_task_output_count: 0,
            max_mesh_work_group_invocations: 0,
            max_mesh_work_group_size: [0; 3],
            max_mesh_total_memory_size: 0,
            max_mesh_output_vertices: 0,
            max_mesh_output_primitives: 0,
            max_mesh_multiview_view_count: 0,
            mesh_output_per_vertex_granularity: 0,
            mesh_output_per_primitive_granularity: 0,
        }
    }

    fn is_valid_cache(&self, cache: &[u8]) -> bool {
        const HEADER_SIZE: usize = 16 + vk::UUID_SIZE;

        if cache.len() < HEADER_SIZE {
            warn!("Bad cache data length {:?}", cache.len());
            return false;
        }

        let header_len = u32::from_le_bytes([cache[0], cache[1], cache[2], cache[3]]);
        let header_version = u32::from_le_bytes([cache[4], cache[5], cache[6], cache[7]]);
        let vendor_id = u32::from_le_bytes([cache[8], cache[9], cache[10], cache[11]]);
        let device_id = u32::from_le_bytes([cache[12], cache[13], cache[14], cache[15]]);

        // header length
        if (header_len as usize) < HEADER_SIZE {
            warn!("Bad header length {:?}", header_len);
            return false;
        }

        // cache header version
        if header_version != vk::PipelineCacheHeaderVersion::ONE.as_raw() as u32 {
            warn!("Unsupported cache header version: {:?}", header_version);
            return false;
        }

        // vendor id
        if vendor_id != self.properties.vendor_id {
            warn!(
                "Vendor ID mismatch. Device: {:?}, cache: {:?}.",
                self.properties.vendor_id, vendor_id,
            );
            return false;
        }

        // device id
        if device_id != self.properties.device_id {
            warn!(
                "Device ID mismatch. Device: {:?}, cache: {:?}.",
                self.properties.device_id, device_id,
            );
            return false;
        }

        if self.properties.pipeline_cache_uuid != cache[16..16 + vk::UUID_SIZE] {
            warn!(
                "Pipeline cache UUID mismatch. Device: {:?}, cache: {:?}.",
                self.properties.pipeline_cache_uuid,
                &cache[16..16 + vk::UUID_SIZE],
            );
            return false;
        }
        true
    }
}

struct DeviceExtensionFunctions {
    mesh_shaders: Option<MeshShader>,
    draw_indirect_count: Option<DrawIndirectCount>,
}

#[doc(hidden)]
pub struct RawDevice {
    raw: ash::Device,
    features: Features,
    instance: Arc<RawInstance>,
    extension_fns: DeviceExtensionFunctions,
    maintenance_level: u8,
    imageless_framebuffers: bool,
}

impl fmt::Debug for RawDevice {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "RawDevice") // TODO: Real Debug impl
    }
}
impl Drop for RawDevice {
    fn drop(&mut self) {
        unsafe {
            self.raw.destroy_device(None);
        }
    }
}

impl RawDevice {
    fn debug_messenger(&self) -> Option<&DebugMessenger> {
        self.instance.debug_messenger.as_ref()
    }

    fn map_viewport(&self, rect: &hal::pso::Viewport) -> vk::Viewport {
        let flip_y = self.features.contains(hal::Features::NDC_Y_UP);
        let shift_y = flip_y && self.maintenance_level != 0;
        conv::map_viewport(rect, flip_y, shift_y)
    }

    unsafe fn set_object_name(
        &self,
        object_type: vk::ObjectType,
        object: impl vk::Handle,
        name: &str,
    ) {
        let instance = &self.instance;
        if let Some(DebugMessenger::Utils(ref debug_utils_ext, _)) = instance.debug_messenger {
            // Keep variables outside the if-else block to ensure they do not
            // go out of scope while we hold a pointer to them
            let mut buffer: [u8; 64] = [0u8; 64];
            let buffer_vec: Vec<u8>;

            // Append a null terminator to the string
            let name_cstr = if name.len() < 64 {
                // Common case, string is very small. Allocate a copy on the stack.
                std::ptr::copy_nonoverlapping(name.as_ptr(), buffer.as_mut_ptr(), name.len());
                // Add null terminator
                buffer[name.len()] = 0;
                CStr::from_bytes_with_nul(&buffer[..name.len() + 1]).unwrap()
            } else {
                // Less common case, the string is large.
                // This requires a heap allocation.
                buffer_vec = name
                    .as_bytes()
                    .iter()
                    .cloned()
                    .chain(std::iter::once(0))
                    .collect::<Vec<u8>>();
                CStr::from_bytes_with_nul(&buffer_vec).unwrap()
            };
            let _result = debug_utils_ext.debug_utils_set_object_name(
                self.raw.handle(),
                &vk::DebugUtilsObjectNameInfoEXT::builder()
                    .object_type(object_type)
                    .object_handle(object.as_raw())
                    .object_name(name_cstr),
            );
        }
    }
}

// Need to explicitly synchronize on submission and present.
pub type RawCommandQueue = Arc<vk::Queue>;

pub struct CommandQueue {
    raw: RawCommandQueue,
    device: Arc<RawDevice>,
    swapchain_fn: Swapchain,
}

impl fmt::Debug for CommandQueue {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("CommandQueue")
    }
}

impl queue::CommandQueue<Backend> for CommandQueue {
    unsafe fn submit<'a, T, Ic, S, Iw, Is>(
        &mut self,
        submission: queue::Submission<Ic, Iw, Is>,
        fence: Option<&mut native::Fence>,
    ) where
        T: 'a + Borrow<command::CommandBuffer>,
        Ic: IntoIterator<Item = &'a T>,
        S: 'a + Borrow<native::Semaphore>,
        Iw: IntoIterator<Item = (&'a S, PipelineStage)>,
        Is: IntoIterator<Item = &'a S>,
    {
        //TODO: avoid heap allocations
        let mut waits = Vec::new();
        let mut stages = Vec::new();

        let buffers = submission
            .command_buffers
            .into_iter()
            .map(|cmd| cmd.borrow().raw)
            .collect::<Vec<_>>();
        for (semaphore, stage) in submission.wait_semaphores {
            waits.push(semaphore.borrow().0);
            stages.push(conv::map_pipeline_stage(stage));
        }
        let signals = submission
            .signal_semaphores
            .into_iter()
            .map(|semaphore| semaphore.borrow().0)
            .collect::<Vec<_>>();

        let mut info = vk::SubmitInfo::builder()
            .wait_semaphores(&waits)
            .command_buffers(&buffers)
            .signal_semaphores(&signals);
        // If count is zero, AMD driver crashes if nullptr is not set for stage masks
        if !stages.is_empty() {
            info = info.wait_dst_stage_mask(&stages);
        }

        let fence_raw = fence.map(|fence| fence.0).unwrap_or(vk::Fence::null());

        let result = self.device.raw.queue_submit(*self.raw, &[*info], fence_raw);
        assert_eq!(Ok(()), result);
    }

    unsafe fn present(
        &mut self,
        surface: &mut window::Surface,
        image: window::SurfaceImage,
        wait_semaphore: Option<&mut native::Semaphore>,
    ) -> Result<Option<Suboptimal>, PresentError> {
        let ssc = surface.swapchain.as_ref().unwrap();
        let wait_semaphore = if let Some(wait_semaphore) = wait_semaphore {
            wait_semaphore.0
        } else {
            let signals = &[ssc.semaphore.0];
            let submit_info = vk::SubmitInfo::builder().signal_semaphores(signals);
            self.device
                .raw
                .queue_submit(*self.raw, &[*submit_info], vk::Fence::null())
                .unwrap();
            ssc.semaphore.0
        };

        let wait_semaphores = &[wait_semaphore];
        let swapchains = &[ssc.swapchain.raw];
        let image_indices = &[image.index];
        let present_info = vk::PresentInfoKHR::builder()
            .wait_semaphores(wait_semaphores)
            .swapchains(swapchains)
            .image_indices(image_indices);

        match self.swapchain_fn.queue_present(*self.raw, &present_info) {
            Ok(true) => Ok(None),
            Ok(false) => Ok(Some(Suboptimal)),
            Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY) => Err(OutOfMemory::Host.into()),
            Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY) => Err(OutOfMemory::Device.into()),
            Err(vk::Result::ERROR_DEVICE_LOST) => Err(DeviceLost.into()),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Err(OutOfDate.into()),
            Err(vk::Result::ERROR_SURFACE_LOST_KHR) => Err(SurfaceLost.into()),
            _ => panic!("Failed to present frame"),
        }
    }

    fn wait_idle(&mut self) -> Result<(), OutOfMemory> {
        match unsafe { self.device.raw.queue_wait_idle(*self.raw) } {
            Ok(()) => Ok(()),
            Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY) => Err(OutOfMemory::Host),
            Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY) => Err(OutOfMemory::Device),
            Err(_) => unreachable!(),
        }
    }
}

#[derive(Debug)]
pub struct Device {
    shared: Arc<RawDevice>,
    vendor_id: u32,
    valid_ash_memory_types: u32,
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum Backend {}
impl hal::Backend for Backend {
    type Instance = Instance;
    type PhysicalDevice = PhysicalDevice;
    type Device = Device;
    type Surface = window::Surface;

    type QueueFamily = QueueFamily;
    type CommandQueue = CommandQueue;
    type CommandBuffer = command::CommandBuffer;

    type Memory = native::Memory;
    type CommandPool = pool::RawCommandPool;

    type ShaderModule = native::ShaderModule;
    type RenderPass = native::RenderPass;
    type Framebuffer = native::Framebuffer;

    type Buffer = native::Buffer;
    type BufferView = native::BufferView;
    type Image = native::Image;
    type ImageView = native::ImageView;
    type Sampler = native::Sampler;

    type ComputePipeline = native::ComputePipeline;
    type GraphicsPipeline = native::GraphicsPipeline;
    type PipelineLayout = native::PipelineLayout;
    type PipelineCache = native::PipelineCache;
    type DescriptorSetLayout = native::DescriptorSetLayout;
    type DescriptorPool = native::DescriptorPool;
    type DescriptorSet = native::DescriptorSet;

    type Fence = native::Fence;
    type Semaphore = native::Semaphore;
    type Event = native::Event;
    type QueryPool = native::QueryPool;
}
