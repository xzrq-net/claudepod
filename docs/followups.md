# Followups

## 1:1 project path mapping

Revisit host-to-guest project paths. The clean shape is to have podman mount the
selected host root at a static staging path, then have the entry process
bind-mount it into the same absolute path used on the host, refusing reserved or
conflicting targets.