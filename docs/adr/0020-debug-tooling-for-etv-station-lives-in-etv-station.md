# ADR-0020: Debug tooling for etv-station lives in etv-station

Any tool for inspecting, explaining, or tuning etv-station's own behavior lives inside the etv-station repository, never inside a dependency's own tooling. A dependency etv-station reads — plex-db-ex and its vendored plexdb-reader copy today, any future one — carries no code, endpoint, or running service aware that etv-station is one of its consumers.
