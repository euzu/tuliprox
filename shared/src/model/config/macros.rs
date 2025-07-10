#[macro_export]
macro_rules! check_input_credentials {
    ($this:ident, $input_type:expr, $definition:expr ) => {
     match $input_type {
            InputType::M3u | InputType::M3uBatch => {
                if $this.username.is_some() || $this.password.is_some() {
                    debug!("for input type m3u: username and password are ignored");
                }
                if $this.username.is_none() && $this.password.is_none() {
                    let (username, password) = get_credentials_from_url_str(&$this.url);
                    $this.username = username;
                    $this.password = password;
                }
            }
            InputType::Xtream => {
                if $this.username.is_none() || $this.password.is_none() {
                    return Err(info_err!("for input type xtream: username and password are mandatory".to_string()));
                }
            }
            InputType::XtreamBatch => {
                if $definition {
                  if $this.url.trim().is_empty() {
                        return Err(info_err!("for input type xtream-batch: url is mandatory".to_string()));
                  }
                } else if $this.username.is_none() || $this.password.is_none() {
                    return Err(info_err!("for input type xtream-batch: username and password are mandatory".to_string()));
                }
            }

        }
    };
}

pub use check_input_credentials;