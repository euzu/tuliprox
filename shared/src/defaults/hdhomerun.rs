//! HDHomerun device defaults.

const DEFAULT_FRIENDLY_NAME: &str = "TuliproxTV";
const DEFAULT_MANUFACTURER: &str = "Silicondust";
const DEFAULT_MODEL_NAME: &str = "HDTC-2US";
const DEFAULT_FIRMWARE_NAME: &str = "hdhomeruntc_atsc";
const DEFAULT_FIRMWARE_VERSION: &str = "20170930";
const DEFAULT_DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaServer:1";
const DEFAULT_DEVICE_UDN: &str =
    "uuid:12345678-90ab-cdef-1234-567890abcdef::urn:dial-multicast:com.silicondust.hdhomerun";

pub fn default_friendly_name() -> String { DEFAULT_FRIENDLY_NAME.into() }
pub fn default_manufacturer() -> String { DEFAULT_MANUFACTURER.into() }
pub fn default_model_name() -> String { DEFAULT_MODEL_NAME.into() }
pub fn default_firmware_name() -> String { DEFAULT_FIRMWARE_NAME.into() }
pub fn default_firmware_version() -> String { DEFAULT_FIRMWARE_VERSION.into() }
pub fn default_device_type() -> String { DEFAULT_DEVICE_TYPE.into() }
pub fn default_device_udn() -> String { DEFAULT_DEVICE_UDN.into() }

pub fn is_default_friendly_name(value: &String) -> bool { value == DEFAULT_FRIENDLY_NAME }
pub fn is_default_manufacturer(value: &String) -> bool { value == DEFAULT_MANUFACTURER }
pub fn is_default_model_name(value: &String) -> bool { value == DEFAULT_MODEL_NAME }
pub fn is_default_firmware_name(value: &String) -> bool { value == DEFAULT_FIRMWARE_NAME }
pub fn is_default_firmware_version(value: &String) -> bool { value == DEFAULT_FIRMWARE_VERSION }
pub fn is_default_device_type(value: &String) -> bool { value == DEFAULT_DEVICE_TYPE }
pub fn is_default_device_udn(value: &String) -> bool { value == DEFAULT_DEVICE_UDN }
