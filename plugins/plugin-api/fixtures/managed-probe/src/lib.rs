use tuliprox_plugin_sdk::prelude::*;
#[tuliprox_plugin]
mod plugin {
    use super::*;
    pub struct Probe;
    impl Rpc for Probe {
        fn call(_method: &str, payload: &str) -> Result<String, String> {
            kv::set_str("state", payload)?;
            Ok("{}".into())
        }
    }
    impl ManagedChannelHooks for Probe {
        fn reconcile_input(input: &str) -> Result<Vec<ManagedChannel>, String> {
            let state: serde_json::Value = serde_json::from_str(&kv::get_str("state")?.unwrap_or_else(|| "{}".into()))
                .map_err(|e| e.to_string())?;
            let destination = state["input"].as_str().unwrap_or("provider");
            if input != destination {
                return Ok(vec![]);
            }
            match state["mode"].as_str().unwrap_or("") {
                "fail" => return Err("simulated outage".into()),
                "trap" => panic!("simulated trap"),
                "spin" => loop {
                    std::hint::black_box(1u64);
                },
                "empty" => return Ok(vec![]),
                _ => {}
            }
            let count = state["count"].as_u64().unwrap_or(2);
            Ok((0..count)
                .map(|index| ManagedChannel {
                    id: if state["duplicate"].as_bool().unwrap_or(false) {
                        "same".into()
                    } else {
                        format!("team-{index}")
                    },
                    value: Channel {
                        handle: 999,
                        input_name: "spoof".into(),
                        input_stream_id: "provider-id-spoof".into(),
                        name: format!("{} {index}", state["label"].as_str().unwrap_or("Team")),
                        title: String::new(),
                        group: "Teamarr".into(),
                        url: format!(
                            "https://streams.test/{}/{index}.ts",
                            state["variant"].as_str().unwrap_or("original")
                        ),
                        epg_id: Some(format!("teamarr-{index}")),
                        logo: String::new(),
                        chno: Some(5000 + index as u32),
                        item_type: ItemType::Live,
                        cluster: Cluster::Live,
                        extra: vec![],
                    },
                })
                .collect())
        }
    }
}
