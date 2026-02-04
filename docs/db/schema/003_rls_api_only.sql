-- Row Level Security: API-Only Access
--
-- This migration locks down direct table access. All registry operations
-- must go through the Registry API service, which uses the service role key.
--
-- The service role key bypasses RLS, so the API can still read/write.
-- Direct client access (using anon key) is blocked for write operations.

-- ============================================================================
-- DIRECTIVES: Read-only for anon, write via service role only
-- ============================================================================

ALTER TABLE directives ENABLE ROW LEVEL SECURITY;
ALTER TABLE directive_versions ENABLE ROW LEVEL SECURITY;

-- Public can read published directives
CREATE POLICY "directives_select_public" ON directives
    FOR SELECT USING (true);

CREATE POLICY "directive_versions_select_public" ON directive_versions
    FOR SELECT USING (true);

-- No direct insert/update/delete for anon - must go through API
-- Service role bypasses RLS automatically

-- ============================================================================
-- TOOLS: Read-only for anon, write via service role only
-- ============================================================================

ALTER TABLE tools ENABLE ROW LEVEL SECURITY;
ALTER TABLE tool_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE tool_version_files ENABLE ROW LEVEL SECURITY;

-- Public can read published tools
CREATE POLICY "tools_select_public" ON tools
    FOR SELECT USING (visibility = 'public' OR visibility IS NULL);

CREATE POLICY "tool_versions_select_public" ON tool_versions
    FOR SELECT USING (true);

CREATE POLICY "tool_version_files_select_public" ON tool_version_files
    FOR SELECT USING (true);

-- ============================================================================
-- KNOWLEDGE: Read-only for anon, write via service role only
-- ============================================================================

ALTER TABLE knowledge ENABLE ROW LEVEL SECURITY;
ALTER TABLE knowledge_versions ENABLE ROW LEVEL SECURITY;

-- Public can read knowledge entries
CREATE POLICY "knowledge_select_public" ON knowledge
    FOR SELECT USING (true);

CREATE POLICY "knowledge_versions_select_public" ON knowledge_versions
    FOR SELECT USING (true);

-- ============================================================================
-- USERS: Read public info, write via service role only
-- ============================================================================

ALTER TABLE users ENABLE ROW LEVEL SECURITY;
ALTER TABLE user_stats ENABLE ROW LEVEL SECURITY;
ALTER TABLE user_activity ENABLE ROW LEVEL SECURITY;

-- Public can read user profiles
CREATE POLICY "users_select_public" ON users
    FOR SELECT USING (true);

CREATE POLICY "user_stats_select_public" ON user_stats
    FOR SELECT USING (true);

-- User activity is private - only service role can access
-- (no SELECT policy means denied for anon)

-- ============================================================================
-- OTHER TABLES: Appropriate access levels
-- ============================================================================

-- Favorites: Users can only see their own
ALTER TABLE favorites ENABLE ROW LEVEL SECURITY;

CREATE POLICY "favorites_select_own" ON favorites
    FOR SELECT USING (auth.uid() = user_id);

-- Follows: Public read
ALTER TABLE follows ENABLE ROW LEVEL SECURITY;

CREATE POLICY "follows_select_public" ON follows
    FOR SELECT USING (true);

-- Signing keys: Only own keys visible
ALTER TABLE signing_keys ENABLE ROW LEVEL SECURITY;

CREATE POLICY "signing_keys_select_own" ON signing_keys
    FOR SELECT USING (auth.uid() = user_id);

-- Bundles: Public read
ALTER TABLE bundles ENABLE ROW LEVEL SECURITY;

CREATE POLICY "bundles_select_public" ON bundles
    FOR SELECT USING (true);

-- Item embeddings: Public read (for search)
ALTER TABLE item_embeddings ENABLE ROW LEVEL SECURITY;

CREATE POLICY "item_embeddings_select_public" ON item_embeddings
    FOR SELECT USING (true);
