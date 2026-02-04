-- Rye Registry Initial Schema
-- Migrated from Agent Kiwi project

-- Enable extensions
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS vector;

-- ============================================================================
-- USERS
-- ============================================================================
CREATE TABLE users (
    id uuid PRIMARY KEY DEFAULT uuid_generate_v4(),
    username text NOT NULL UNIQUE,
    email text UNIQUE,
    created_at timestamptz DEFAULT now(),
    updated_at timestamptz DEFAULT now()
);

-- ============================================================================
-- USER STATS
-- ============================================================================
CREATE TABLE user_stats (
    user_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    total_executions integer DEFAULT 0,
    total_published integer DEFAULT 0,
    total_downloads integer DEFAULT 0,
    items_used integer DEFAULT 0,
    member_since timestamptz DEFAULT now(),
    last_active timestamptz DEFAULT now(),
    streak_days integer DEFAULT 0,
    contribution_score numeric DEFAULT 0
);

CREATE TABLE user_activity (
    user_id uuid REFERENCES users(id) ON DELETE CASCADE,
    item_type text NOT NULL CHECK (item_type IN ('directive', 'tool', 'knowledge')),
    executions integer DEFAULT 0,
    successes integer DEFAULT 0,
    publishes integer DEFAULT 0,
    downloads_received integer DEFAULT 0,
    PRIMARY KEY (user_id, item_type)
);

CREATE INDEX idx_user_stats_contribution ON user_stats(contribution_score DESC);
CREATE INDEX idx_user_stats_last_active ON user_stats(last_active);

-- ============================================================================
-- DIRECTIVES
-- ============================================================================
CREATE TABLE directives (
    id uuid PRIMARY KEY DEFAULT uuid_generate_v4(),
    name text NOT NULL UNIQUE,
    category text NOT NULL,
    description text,
    author_id uuid REFERENCES users(id),
    is_official boolean DEFAULT false,
    download_count integer DEFAULT 0,
    execution_count integer DEFAULT 0,
    dependencies jsonb DEFAULT '[]'::jsonb,
    tags jsonb DEFAULT '[]'::jsonb,
    search_vector tsvector,
    created_at timestamptz DEFAULT now(),
    updated_at timestamptz DEFAULT now()
);

CREATE TABLE directive_versions (
    id uuid PRIMARY KEY DEFAULT uuid_generate_v4(),
    directive_id uuid NOT NULL REFERENCES directives(id) ON DELETE CASCADE,
    version text NOT NULL CHECK (is_valid_semver(version)),
    content text NOT NULL,
    content_hash text NOT NULL,
    changelog text,
    is_latest boolean DEFAULT false,
    created_at timestamptz DEFAULT now(),
    UNIQUE(directive_id, version)
);

-- Indexes
CREATE INDEX idx_directives_category ON directives(category);
CREATE INDEX idx_directives_name ON directives(name);
CREATE INDEX idx_directives_author_id ON directives(author_id);
CREATE INDEX directives_search_idx ON directives USING gin(search_vector);
CREATE INDEX directives_name_idx ON directives(lower(name));
CREATE INDEX directives_description_idx ON directives USING gin(to_tsvector('english', COALESCE(description, '')));
CREATE INDEX idx_directives_tags ON directives USING gin(tags);
CREATE INDEX idx_directive_versions_directive_id ON directive_versions(directive_id);
CREATE INDEX idx_directive_versions_latest ON directive_versions(directive_id) WHERE is_latest = true;

-- ============================================================================
-- TOOLS
-- ============================================================================
CREATE TABLE tools (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_id text NOT NULL,
    namespace text DEFAULT 'public',
    name text,
    tool_type text NOT NULL,
    category text,
    description text,
    tags text[] DEFAULT '{}',
    executor_id text,
    is_builtin boolean DEFAULT false,
    author_id uuid REFERENCES users(id),
    is_official boolean DEFAULT false,
    visibility text DEFAULT 'public' CHECK (visibility IN ('public', 'unlisted', 'private')),
    download_count integer DEFAULT 0,
    execution_count integer DEFAULT 0,
    latest_version text,
    created_at timestamptz DEFAULT now(),
    updated_at timestamptz DEFAULT now(),
    UNIQUE(namespace, tool_id)
);

CREATE TABLE tool_versions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_id uuid NOT NULL REFERENCES tools(id) ON DELETE CASCADE,
    version text NOT NULL CHECK (is_valid_semver(version)),
    manifest jsonb NOT NULL,
    manifest_yaml text,
    content_hash text NOT NULL,
    changelog text,
    is_latest boolean DEFAULT false,
    deprecated boolean DEFAULT false,
    deprecation_message text,
    published_at timestamptz DEFAULT now(),
    created_at timestamptz DEFAULT now(),
    UNIQUE(tool_id, version)
);

CREATE TABLE tool_version_files (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tool_version_id uuid NOT NULL REFERENCES tool_versions(id) ON DELETE CASCADE,
    path text NOT NULL,
    media_type text,
    content_text text,
    storage_key text,
    sha256 text NOT NULL,
    size_bytes integer,
    is_executable boolean DEFAULT false,
    created_at timestamptz DEFAULT now(),
    UNIQUE(tool_version_id, path)
);

-- Indexes
CREATE INDEX idx_tools_tool_id ON tools(tool_id);
CREATE INDEX idx_tools_executor_id ON tools(executor_id);
CREATE INDEX idx_tools_tool_type ON tools(tool_type);
CREATE INDEX idx_tools_category ON tools(category);
CREATE INDEX idx_tools_is_builtin ON tools(is_builtin) WHERE is_builtin = true;
CREATE INDEX idx_tool_versions_tool_id ON tool_versions(tool_id);
CREATE INDEX idx_tool_versions_is_latest ON tool_versions(is_latest) WHERE is_latest = true;
CREATE INDEX idx_tool_versions_content_hash ON tool_versions(content_hash);
CREATE INDEX idx_tool_version_files_version ON tool_version_files(tool_version_id);

-- ============================================================================
-- KNOWLEDGE
-- ============================================================================
CREATE TABLE knowledge (
    id uuid PRIMARY KEY DEFAULT uuid_generate_v4(),
    zettel_id text NOT NULL UNIQUE,
    title text NOT NULL,
    entry_type text NOT NULL CHECK (entry_type IN ('api_fact', 'pattern', 'concept', 'learning', 'experiment', 'reference', 'template', 'workflow')),
    category text,
    source_type text CHECK (source_type IN ('youtube', 'docs', 'experiment', 'manual', 'chat', 'book', 'article', 'course')),
    source_url text,
    tags text[] DEFAULT '{}',
    author_id uuid REFERENCES users(id),
    is_official boolean DEFAULT false,
    download_count integer DEFAULT 0,
    execution_count integer DEFAULT 0,
    search_vector tsvector,
    created_at timestamptz DEFAULT now(),
    updated_at timestamptz DEFAULT now()
);

CREATE TABLE knowledge_versions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    knowledge_id uuid NOT NULL REFERENCES knowledge(id) ON DELETE CASCADE,
    version text NOT NULL CHECK (is_valid_semver(version)),
    content text NOT NULL,
    content_hash text NOT NULL,
    changelog text,
    is_latest boolean DEFAULT false,
    created_at timestamptz DEFAULT now()
);

-- Indexes
CREATE INDEX idx_knowledge_search ON knowledge USING gin(search_vector);
CREATE INDEX idx_knowledge_tags ON knowledge USING gin(tags);
CREATE INDEX idx_knowledge_zettel_id ON knowledge(zettel_id);
CREATE INDEX idx_knowledge_entry_type ON knowledge(entry_type);
CREATE INDEX idx_knowledge_category ON knowledge(category);
CREATE INDEX idx_knowledge_versions_knowledge_id ON knowledge_versions(knowledge_id);
CREATE INDEX idx_knowledge_versions_version ON knowledge_versions(version);
CREATE INDEX idx_knowledge_versions_is_latest ON knowledge_versions(is_latest);

-- ============================================================================
-- EMBEDDINGS (Vector Search)
-- ============================================================================
CREATE TABLE item_embeddings (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    item_id text NOT NULL UNIQUE,
    item_type text NOT NULL CHECK (item_type IN ('directive', 'tool', 'knowledge')),
    embedding vector(1536),
    content text NOT NULL,
    metadata jsonb DEFAULT '{}',
    signature text,
    validated_at timestamptz DEFAULT now(),
    created_at timestamptz DEFAULT now(),
    updated_at timestamptz DEFAULT now()
);

CREATE INDEX idx_item_embeddings_type ON item_embeddings(item_type);
CREATE INDEX idx_item_embeddings_item_id ON item_embeddings(item_id);
CREATE INDEX item_embeddings_embedding_idx ON item_embeddings USING ivfflat(embedding vector_cosine_ops) WITH (lists = 100);

-- ============================================================================
-- FAVORITES (user bookmarks)
-- ============================================================================
CREATE TABLE favorites (
    user_id uuid REFERENCES users(id) ON DELETE CASCADE,
    item_type text NOT NULL CHECK (item_type IN ('directive', 'tool', 'knowledge', 'bundle')),
    item_id text NOT NULL,
    created_at timestamptz DEFAULT now(),
    PRIMARY KEY (user_id, item_type, item_id)
);

-- ============================================================================
-- RATINGS (community quality scores)
-- ============================================================================
CREATE TABLE ratings (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id uuid REFERENCES users(id) ON DELETE CASCADE,
    item_type text NOT NULL CHECK (item_type IN ('directive', 'tool', 'knowledge', 'bundle')),
    item_id text NOT NULL,
    rating smallint NOT NULL CHECK (rating >= 1 AND rating <= 5),
    review text,
    created_at timestamptz DEFAULT now(),
    UNIQUE(user_id, item_type, item_id)
);

CREATE INDEX idx_ratings_item ON ratings(item_type, item_id);

-- ============================================================================
-- REPORTS (flag malicious/broken items)
-- ============================================================================
CREATE TABLE reports (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    reporter_id uuid REFERENCES users(id),
    item_type text NOT NULL CHECK (item_type IN ('directive', 'tool', 'knowledge', 'bundle')),
    item_id text NOT NULL,
    reason text NOT NULL CHECK (reason IN ('malicious', 'broken', 'spam', 'inappropriate', 'other')),
    description text,
    status text DEFAULT 'pending' CHECK (status IN ('pending', 'reviewed', 'resolved', 'dismissed')),
    created_at timestamptz DEFAULT now(),
    resolved_at timestamptz
);

CREATE INDEX idx_reports_status ON reports(status) WHERE status = 'pending';

-- ============================================================================
-- FOLLOWS (subscribe to authors)
-- ============================================================================
CREATE TABLE follows (
    follower_id uuid REFERENCES users(id) ON DELETE CASCADE,
    following_id uuid REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz DEFAULT now(),
    PRIMARY KEY (follower_id, following_id)
);

CREATE INDEX idx_follows_following ON follows(following_id);

-- ============================================================================
-- NEW: SIGNING KEYS (for Lilux registry auth)
-- ============================================================================
CREATE TABLE signing_keys (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key_id text NOT NULL UNIQUE,  -- Short identifier like "leo@main"
    public_key text NOT NULL,      -- PEM-encoded public key
    algorithm text DEFAULT 'ed25519',
    is_primary boolean DEFAULT false,
    created_at timestamptz DEFAULT now(),
    expires_at timestamptz,
    revoked_at timestamptz
);

CREATE INDEX idx_signing_keys_user ON signing_keys(user_id);
CREATE INDEX idx_signing_keys_key_id ON signing_keys(key_id);

-- ============================================================================
-- NEW: BUNDLES (content bundles like demos, starter packs)
-- ============================================================================
CREATE TABLE bundles (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    bundle_id text NOT NULL UNIQUE,  -- e.g., "core/demos/level-1"
    name text NOT NULL,
    description text,
    author_id uuid REFERENCES users(id),
    is_official boolean DEFAULT false,
    items jsonb NOT NULL DEFAULT '[]',  -- Array of {item_type, item_id, version}
    download_count integer DEFAULT 0,
    created_at timestamptz DEFAULT now(),
    updated_at timestamptz DEFAULT now()
);

CREATE INDEX idx_bundles_bundle_id ON bundles(bundle_id);
