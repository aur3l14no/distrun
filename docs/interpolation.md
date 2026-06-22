# Interpolation

distrun lets `serde-saphyr` parse `${...}`. distrun chooses the property map.

The shape is:

1. collect input values
2. build `properties`
3. ask `serde-saphyr` to interpolate
4. apply distrun validation

`interpolate_value` enables `PropertySyntax::BracedOrBare`, so the parser
understands `$KEY`, `${KEY}`, defaults, required values, replacements, and `$$`.

## Pipeline

1. Parent environment
   - Build with `env::vars().collect::<BTreeMap<_, _>>()`.

2. Config paths
   - Fields: `include`, `include?`, `env_file`.
   - Properties: parent environment only.
   - Functions: `interpolate_path_list`, `resolve_env_files`.

3. Project and hosts
   - Fields: `project`, `on_existing`, `hosts.*.ssh`.
   - Properties: parent environment only.
   - Functions: `normalize_project_name`, `normalize_on_existing`,
     `normalize_host_transport`.

4. Service env
   - Inputs: `env_file`, then inline `env` overrides.
   - For `env.KEY`, properties are parent environment plus other service env
     entries; `KEY`'s own raw value is excluded.
   - Function: `interpolate_env`.

5. Service fields
   - Fields: the service body by default, except `env_file` and `env`.
   - Properties: parent environment plus resolved service env; service env wins.
   - Function: `interpolate_service`.

6. Service normalization
   - distrun then applies defaults, validates hosts, and parses durations.
   - Function: `normalize_services`.

## Rule

Do not hide scope inside `interpolate_value`. Callers build the property map.
This is single-pass interpolation; do not add recursive env dependency
resolution unless it becomes an explicit product requirement.
