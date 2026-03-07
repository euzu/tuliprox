#[macro_export]
macro_rules! check_input_credentials {
    ($this:ident, $input_type:expr, $definition:expr, $alias:expr ) => {
        let __tp_input_name = $this.name.to_string();
        let __tp_input_name = __tp_input_name.trim().to_string();
        let __tp_input_name_suffix =
            if __tp_input_name.is_empty() { String::new() } else { format!(" (input: {})", __tp_input_name) };

        if !matches!($input_type, InputType::Library) {
            $this.url = $this.url.trim().to_string();
            if $this.url.is_empty() {
                return info_err_res!("url for input is mandatory{}", __tp_input_name_suffix);
            }

            $this.username = $crate::utils::get_trimmed_string($this.username.as_deref());
            $this.password = $crate::utils::get_trimmed_string($this.password.as_deref());
        }
        match $input_type {
            InputType::M3u => {
                if $this.username.is_some() || $this.password.is_some() {
                    // TODO only for initial check
                    //return Err(info_err!("Input types of m3u should not use username or password"));
                }
                let (username, password) = $crate::utils::get_credentials_from_url_str(&$this.url);
                $this.username = username;
                $this.password = password;
            }
            InputType::M3uBatch => {
                if $definition {
                    if $this.url.trim().is_empty() {
                        return info_err_res!("for input type m3u-batch: url is mandatory{}", __tp_input_name_suffix);
                    }
                }

                // if !$alias && ($this.username.is_some() || $this.password.is_some()) {
                //     // TODO only for initial check
                //    // return Err(info_err!("Input types of m3u-batch should not define username or password"));
                // }
            }
            InputType::Xtream => {
                if $this.username.is_none() || $this.password.is_none() {
                    return info_err_res!(
                        "for input type xtream: username and password are mandatory{}",
                        __tp_input_name_suffix
                    );
                }
            }
            InputType::XtreamBatch => {
                if $definition {
                    if $this.url.trim().is_empty() {
                        return info_err_res!(
                            "for input type xtream-batch: url is mandatory{}",
                            __tp_input_name_suffix
                        );
                    }
                }

                if !$alias {
                    let has_username = $this.username.is_some();
                    let has_password = $this.password.is_some();
                    let has_credentials = has_username || has_password;
                    let is_batch_url = $this.url.starts_with($crate::utils::BATCH_SCHEME_PREFIX);

                    if is_batch_url {
                        if has_credentials {
                            return info_err_res!(
                                "input type xtream-batch with batch:// URL should not define username or password attribute{}",
                                __tp_input_name_suffix
                            );
                        }
                    } else if !has_username || !has_password {
                        return info_err_res!(
                            "for input type xtream-batch without batch:// URL: username and password are mandatory{}",
                            __tp_input_name_suffix
                        );
                    }
                }
            }
            InputType::Library => {
                // nothing to do
            }
        }
    };
}

#[macro_export]
macro_rules! check_input_connections {
    ($this:ident, $input_type:expr, $alias:expr) => {
        let __tp_input_name = $this.name.to_string();
        let __tp_input_name = __tp_input_name.trim().to_string();
        let __tp_input_name_suffix =
            if __tp_input_name.is_empty() { String::new() } else { format!(" (input: {})", __tp_input_name) };

        match $input_type {
            InputType::M3u | InputType::Xtream => {}
            InputType::M3uBatch => {
                if !$alias {
                    if $this.max_connections > 0 {
                        return info_err_res!(
                            "input type m3u-batch should not define max_connections attribute{}",
                            __tp_input_name_suffix
                        );
                    }
                    if $this.priority != 0 {
                        return info_err_res!(
                            "input type m3u-batch should not define priority attribute{}",
                            __tp_input_name_suffix
                        );
                    }
                }
            }
            InputType::XtreamBatch => {
                if !$alias {
                    if $this.max_connections > 0 {
                        return info_err_res!(
                            "input type xtream-batch should not define max_connections attribute{}",
                            __tp_input_name_suffix
                        );
                    }
                    if $this.priority != 0 {
                        return info_err_res!(
                            "input type xtream-batch should not define priority attribute{}",
                            __tp_input_name_suffix
                        );
                    }
                }
            }
            InputType::Library => {}
        }
    };
}

pub use check_input_connections;
pub use check_input_credentials;
