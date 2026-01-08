#[macro_export]
macro_rules! check_input_credentials {
    ($this:ident, $input_type:expr, $definition:expr, $alias:expr ) => {
     if !matches!($input_type, InputType::Library) {

            $this.url = $this.url.trim().to_string();
            if $this.url.is_empty() {
                return info_err_res!("url for input is mandatory");
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
                        return info_err_res!("for input type m3u-batch: url is mandatory");
                  }
                }

                // if !$alias && ($this.username.is_some() || $this.password.is_some()) {
                //     // TODO only for initial check
                //    // return Err(info_err!("Input types of m3u-batch should not define username or password"));
                // }
            }
            InputType::Xtream => {
                if $this.username.is_none() || $this.password.is_none() {
                    return info_err_res!("for input type xtream: username and password are mandatory");
                }
            }
            InputType::XtreamBatch => {
                if $definition {
                  if $this.url.trim().is_empty() {
                        return info_err_res!("for input type xtream-batch: url is mandatory");
                  }
                }

                if !$alias && ($this.username.is_some() || $this.password.is_some()) {
                  return info_err_res!("input type xtream-batch should not define username or password attribute ");
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

     match $input_type {
            InputType::M3u |InputType::Xtream  => {
            }
            InputType::M3uBatch => {
                if !$alias {
                    if $this.max_connections > 0 {
                        return info_err_res!("input type m3u-batch should not define max_connections attribute ");
                    }
                    if $this.priority != 0 {
                        return info_err_res!("input type m3u-batch should not define priority attribute ");
                    }
                }
            }
            InputType::XtreamBatch => {
                if !$alias {
                    if $this.max_connections > 0 {
                        return info_err_res!("input type xtream-batch should not define max_connections attribute ");
                    }
                    if $this.priority != 0 {
                        return info_err_res!("input type xtream-batch should not define priority attribute ");
                    }
                }
            }
            InputType::Library => {}
        }
    };
}

pub use check_input_credentials;
pub use check_input_connections;