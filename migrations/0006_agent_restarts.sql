-- Interrupted runs and images with several repo digests (0.2.1).
--
-- The agent process a run was handed to: the boot id the agent's job poll
-- carried (a random id per agent process). NULL for runs handed out before
-- 0.2.1 or to an older agent. When the agent reports another boot id (it
-- restarted), its running runs are closed as "needs you": nothing finishes
-- them any more.
ALTER TABLE runs ADD COLUMN agent_boot TEXT;

-- Every RepoDigests entry of the image for the container's repository,
-- space separated. An image pulled again by an index digest keeps the old
-- entry too; it is current when any of them is the registry's digest.
ALTER TABLE target_images ADD COLUMN local_digests TEXT NOT NULL DEFAULT '';
