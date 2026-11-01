# River 0.5's synchronized virtual keyboards are not KeyboardGroup objects.
# Preserve their native state instead of casting a null group pointer on focus.
{ river }:
river.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [ ../patches/river-synced-keyboard-focus.patch ];
})
