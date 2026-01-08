use crate::repository::bplustree::BPlusTree;
use std::io;
use std::io::BufRead;
use std::net::Ipv4Addr;
use std::path::Path;

fn ipv4_to_u32(ip: &str) -> Option<u32> {
    ip.parse::<Ipv4Addr>().ok().map(u32::from)
}

pub struct GeoIp {
    tree: BPlusTree<u32, (u32, String)>,
}


impl GeoIp {

    fn seed_private_ranges(tree: &mut BPlusTree<u32, (u32, String)>) {
        /// Private and commonly used reserved IPv4 ranges.
        /// "Docker" subnets reflect typical defaults, not exclusive ownership.
        const PRIVATE_RANGES: [(&str, &str, &str); 8] = [
            ("127.0.0.0", "127.255.255.255", "Loopback"),
            ("10.0.0.0", "10.255.255.255", "LAN"),
            ("172.16.0.0", "172.31.255.255", "LAN"),
            ("192.168.0.0", "192.168.255.255", "LAN"),
            ("169.254.0.0", "169.254.255.255", "Link-Local"),
            // Common Docker networks (subset of 172.16.0.0/12)
            ("172.17.0.0", "172.17.255.255", "Docker"),
            ("172.18.0.0", "172.18.255.255", "Docker"),
            ("172.19.0.0", "172.19.255.255", "Docker"),
        ];

        for (start, end, cc) in PRIVATE_RANGES {
            if let (Some(start), Some(end)) = (ipv4_to_u32(start), ipv4_to_u32(end)) {
                tree.insert(start, (end, cc.to_string()));
            }
        }
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let mut tree = BPlusTree::load(path)?;
        Self::seed_private_ranges(&mut tree);

        Ok(Self { tree })
    }

    pub fn new() -> Self {
        let mut tree = BPlusTree::new();
        Self::seed_private_ranges(&mut tree);
        Self {
            tree
        }
    }

    pub fn import_ipv4_from_csv(&mut self, mut reader: impl BufRead, db_path: &Path) -> std::io::Result<u64> {
        let mut buf = String::new();

        loop {
            buf.clear();
            if reader.read_line(&mut buf)? == 0 { break; }
            let line = buf.trim();
            if line.is_empty() || line.starts_with('#') { continue; }

            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() != 3 { continue; }

            if let (Some(start), Some(end)) = (ipv4_to_u32(parts[0]), ipv4_to_u32(parts[1])) {
                let cc = parts[2].trim().to_string();
                self.tree.insert(start, (end, cc));
            }
            buf.clear();
        }
        self.tree.store(db_path)
    }

    pub fn lookup(&self, ip_str: &str) -> Option<String> {
        let ip = ipv4_to_u32(ip_str)?;
        if let Some((_, (end, cc))) = self.tree.find_le(&ip) {
            if ip <= *end {
                return Some(cc.clone());
            }
        }
        None
    }
}

impl Default for GeoIp {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod test {
    // https://raw.githubusercontent.com/sapics/ip-location-db/refs/heads/main/asn-country/asn-country-ipv4.csv

    // use crate::utils::geoip::GeoIp;
    // use std::fs::File;
    // use std::path::PathBuf;
    // use crate::utils::file_reader;

    // #[test]
    // pub fn test_csv() {
    //     let db_file = PathBuf::from("/projects/m3u-test/asn-country-ipv4.db");
    //     let source = PathBuf::from("/projects/m3u-test/asn-country-ipv4.csv");
    //     let file = File::open(source).expect("Could not open csv file");
    //     let reader = file_reader(file);
    //     let mut geo_ip = GeoIp::new();
    //     let _ = geo_ip.import_ipv4_from_csv(reader, &db_file).expect("Could not import csv");
    //
    //     let geo_ip = GeoIp::load(&db_file).expect("Failed to load geoip db");
    //     if let Some(cc) = geo_ip.lookup("72.13.24.23") {
    //         assert_eq!(cc, "US");
    //     } else {
    //         panic!("GeoIP lookup returned no result");
    //     }
    // }
}
