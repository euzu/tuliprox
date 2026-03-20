Sample configuration.

## Login user credentials

The `user.txt` file uses the format `username:argon2_hash[:group1,group2,...]`:

- `test:test.secret` (no groups = defaults to `admin`)
- `nobody:nobody.secret`

Generate password hashes with `tuliprox --genpwd`.

## RBAC groups

The optional `groups.txt` file defines permission groups in `group_name:permission1,permission2,...` format.
See the [Config Reference](docs/src/configuration/main-config.md) for the full permission list and file format.
