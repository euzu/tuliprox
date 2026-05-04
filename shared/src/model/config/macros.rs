#[macro_export]
macro_rules! check_input_credentials {
    ($this:ident, $input_type:expr, $definition:expr, $alias:expr ) => {
        let input_name = $this.name.to_string();
        let input_name = input_name.trim().to_string();
        let input_name_suffix =
            if input_name.is_empty() { String::new() } else { format!(" (input: {input_name})") };

        if !matches!($input_type, InputType::Library) {
            $this.url = $this.url.trim().to_string();
            // This generic check only applies to classic URL-backed playlist inputs. Media-server
            // inputs have provider-specific URL/discovery validation in their prepare path.
            if $input_type.uses_standard_input_url() && $this.url.is_empty() {
                return Err($crate::error::TuliproxError::ConfigInput(format!("url for input is mandatory{input_name_suffix}")));
            }

            $this.username = $crate::utils::get_trimmed_string($this.username.as_deref());
            $this.password = $crate::utils::get_trimmed_string($this.password.as_deref());
        }
        match $input_type {
            InputType::M3u => {
                if $this.username.is_some() || $this.password.is_some() {
                    // TODO only for initial check
                    //return Err(TuliproxError::ConfigInput(format!("Input types of m3u should not use username or password")));
                }
                let (username, password) = $crate::utils::get_credentials_from_url_str(&$this.url);
                $this.username = username;
                $this.password = password;
            }
            InputType::M3uBatch => {
                if $definition {
                    if $this.url.trim().is_empty() {
                        return Err($crate::error::TuliproxError::ConfigInput(format!("for input type m3u-batch: url is mandatory{input_name_suffix}")));
                    }
                }

                // if !$alias && ($this.username.is_some() || $this.password.is_some()) {
                //     // TODO only for initial check
                //    // return Err(TuliproxError::ConfigInput(format!("Input types of m3u-batch should not define username or password")));
                // }
            }
            InputType::Xtream => {
                if $this.username.is_none() || $this.password.is_none() {
                    return Err($crate::error::TuliproxError::ConfigInput(format!(
                        "for input type xtream: username and password are mandatory{input_name_suffix}",
                    )));
                }
            }
            InputType::XtreamBatch => {
                if $definition {
                    if $this.url.trim().is_empty() {
                        return Err($crate::error::TuliproxError::ConfigInput(format!(
                            "for input type xtream-batch: url is mandatory{input_name_suffix}",
                        )));
                    }
                }

                if !$alias {
                    let has_username = $this.username.is_some();
                    let has_password = $this.password.is_some();
                    let has_credentials = has_username || has_password;
                    let is_batch_url = $this.url.starts_with($crate::utils::BATCH_SCHEME_PREFIX);

                    if is_batch_url {
                        if has_credentials {
                            return Err($crate::error::TuliproxError::ConfigInput(format!(
                                "input type xtream-batch with batch:// URL should not define username or password attribute{input_name_suffix}",
                            )));
                        }
                    } else if !has_username || !has_password {
                        return Err($crate::error::TuliproxError::ConfigInput(format!(
                            "for input type xtream-batch without batch:// URL: username and password are mandatory{input_name_suffix}",
                        )));
                    }
                }
            }
            InputType::Library | InputType::Emby | InputType::Jellyfin | InputType::Plex => {
                // Media-server credentials live in the dedicated media_server block; detailed
                // validation happens in ConfigInputDto/ConfigInput prepare methods.
            }
        }
    };
}

#[macro_export]
macro_rules! check_input_connections {
    ($this:ident, $input_type:expr, $alias:expr) => {
        let input_name = $this.name.to_string();
        let input_name = input_name.trim().to_string();
        let input_name_suffix = if input_name.is_empty() { String::new() } else { format!(" (input: {input_name})") };

        match $input_type {
            InputType::M3u | InputType::Xtream => {}
            InputType::M3uBatch => {
                if !$alias {
                    if $this.max_connections > 0 {
                        return Err($crate::error::TuliproxError::ConfigInput(format!(
                            "input type m3u-batch should not define max_connections attribute{input_name_suffix}",
                        )));
                    }
                    if $this.priority != 0 {
                        return Err($crate::error::TuliproxError::ConfigInput(format!(
                            "input type m3u-batch should not define priority attribute{input_name_suffix}",
                        )));
                    }
                }
            }
            InputType::XtreamBatch => {
                if !$alias {
                    if $this.max_connections > 0 {
                        return Err($crate::error::TuliproxError::ConfigInput(format!(
                            "input type xtream-batch should not define max_connections attribute{input_name_suffix}",
                        )));
                    }
                    if $this.priority != 0 {
                        return Err($crate::error::TuliproxError::ConfigInput(format!(
                            "input type xtream-batch should not define priority attribute{input_name_suffix}",
                        )));
                    }
                }
            }
            InputType::Library | InputType::Emby | InputType::Jellyfin | InputType::Plex => {}
        }
    };
}

pub use check_input_connections;
pub use check_input_credentials;
