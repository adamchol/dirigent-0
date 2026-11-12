export HOME=/tmp NIX_CONFIG="experimental-features = nix-command"
# the players' cache, as the VMs read it (its `profile=dirigent`)
mkdir -p ~/.aws
printf '[dirigent]\naws_access_key_id = %s\naws_secret_access_key = %s\n' \
  "$S3_ACCESS_KEY" "$S3_SECRET_KEY" > ~/.aws/credentials
cd /store
new=gen-$GENERATION
rm -rf "$new.tmp"
mkdir "$new.tmp"
if [ -n "$PREVIOUS" ]; then
  # the last one's paths, the same files; its database its own
  cp -al "gen-$PREVIOUS/nix" "$new.tmp/nix"
  rm -rf "$new.tmp/nix/var/nix/db"
  cp -a "gen-$PREVIOUS/nix/var/nix/db" "$new.tmp/nix/var/nix/db"
fi
store="local?root=/store/$new.tmp"
nix store info --store "$store" > /dev/null
added=0
left=0
while read -r kind path; do
  if [ "$kind" = signed ]; then
    from=(--from https://cache.nixos.org)
  else
    from=(--from "$NIX_CACHE" --no-check-sigs)
  fi
  if nix copy "${from[@]}" --to "$store" "$path"; then
    added=$((added + 1))
  else
    echo "left out: $path" >&2
    left=$((left + 1))
  fi
done < "$new.todo"
# read by the VMs' virtiofs, which isn't root
chmod -R a+rX "$new.tmp/nix/var"
mv "$new.tmp" "$new"
touch "$new/complete"
echo "generation $GENERATION: $added paths added, $left left out"
