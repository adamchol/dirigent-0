-- Until now a generation's paths were all marked as in it, those its writer couldn't get too
-- (and before 0.1.8 it couldn't get any from cache.nixos.org): every path is tried again, by the
-- next generation, which marks only those it has.
UPDATE "dirigent"."nix_paths" SET "promoted_in" = NULL;
