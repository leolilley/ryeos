-- Rye Registry Functions
-- Migrated from Agent Kiwi project

-- ============================================================================
-- UTILITY FUNCTIONS
-- ============================================================================

CREATE OR REPLACE FUNCTION is_valid_semver(version text)
RETURNS boolean
LANGUAGE plpgsql IMMUTABLE
AS $$
BEGIN
    RETURN version ~ '^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?(\+[a-zA-Z0-9.-]+)?$';
END;
$$;

CREATE OR REPLACE FUNCTION update_updated_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$;

-- ============================================================================
-- SEARCH FUNCTIONS
-- ============================================================================

CREATE OR REPLACE FUNCTION search_embeddings(
    query_embedding vector(1536),
    filter_type text DEFAULT NULL,
    match_count integer DEFAULT 10
)
RETURNS TABLE(
    item_id text,
    item_type text,
    content text,
    metadata jsonb,
    similarity float
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
BEGIN
    RETURN QUERY
    SELECT
        ie.item_id,
        ie.item_type,
        ie.content,
        ie.metadata,
        1 - (ie.embedding <=> query_embedding) AS similarity
    FROM item_embeddings ie
    WHERE (filter_type IS NULL OR ie.item_type = filter_type)
    ORDER BY ie.embedding <=> query_embedding
    LIMIT match_count;
END;
$$;

CREATE OR REPLACE FUNCTION search_tools(
    p_query text DEFAULT NULL,
    p_tool_type text DEFAULT NULL,
    p_category text DEFAULT NULL,
    p_limit integer DEFAULT 20
)
RETURNS TABLE(
    id uuid,
    tool_id text,
    name text,
    tool_type text,
    category text,
    description text,
    executor_id text,
    latest_version text,
    rank real
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
BEGIN
    RETURN QUERY
    SELECT 
        t.id,
        t.tool_id,
        t.name,
        t.tool_type,
        t.category,
        t.description,
        t.executor_id,
        t.latest_version,
        ts_rank(
            to_tsvector('english', coalesce(t.name, '') || ' ' || coalesce(t.description, '') || ' ' || coalesce(t.tool_id, '')),
            plainto_tsquery('english', p_query)
        ) AS rank
    FROM public.tools t
    WHERE 
        (p_tool_type IS NULL OR t.tool_type = p_tool_type)
        AND (p_category IS NULL OR t.category = p_category)
        AND (
            p_query IS NULL 
            OR p_query = ''
            OR to_tsvector('english', coalesce(t.name, '') || ' ' || coalesce(t.description, '') || ' ' || coalesce(t.tool_id, ''))
               @@ plainto_tsquery('english', p_query)
            OR t.tool_id ILIKE '%' || p_query || '%'
            OR t.name ILIKE '%' || p_query || '%'
        )
    ORDER BY rank DESC, t.download_count DESC NULLS LAST
    LIMIT p_limit;
END;
$$;

-- ============================================================================
-- KNOWLEDGE FUNCTIONS
-- ============================================================================

CREATE OR REPLACE FUNCTION update_knowledge_search_vector()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.search_vector := to_tsvector('english', 
        COALESCE(NEW.title, '') || ' ' || 
        COALESCE(NEW.category, '') || ' ' ||
        COALESCE(array_to_string(NEW.tags, ' '), '')
    );
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION get_knowledge_with_content(p_zettel_id text)
RETURNS TABLE(
    id uuid,
    zettel_id text,
    title text,
    entry_type text,
    category text,
    source_type text,
    source_url text,
    tags text[],
    author_id uuid,
    is_official boolean,
    download_count integer,
    created_at timestamptz,
    updated_at timestamptz,
    version text,
    content text,
    content_hash text
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
BEGIN
    RETURN QUERY
    SELECT 
        k.id,
        k.zettel_id,
        k.title,
        k.entry_type,
        k.category,
        k.source_type,
        k.source_url,
        k.tags,
        k.author_id,
        k.is_official,
        k.download_count,
        k.created_at,
        k.updated_at,
        v.version,
        v.content,
        v.content_hash
    FROM knowledge k
    LEFT JOIN knowledge_versions v ON v.knowledge_id = k.id AND v.is_latest = true
    WHERE k.zettel_id = p_zettel_id;
END;
$$;

CREATE OR REPLACE FUNCTION list_knowledge_with_versions(
    p_category text DEFAULT NULL,
    p_entry_type text DEFAULT NULL,
    p_limit integer DEFAULT 50
)
RETURNS TABLE(
    zettel_id text,
    title text,
    entry_type text,
    category text,
    tags text[],
    version text,
    created_at timestamptz,
    updated_at timestamptz
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
BEGIN
    RETURN QUERY
    SELECT 
        k.zettel_id,
        k.title,
        k.entry_type,
        k.category,
        k.tags,
        v.version,
        k.created_at,
        k.updated_at
    FROM knowledge k
    LEFT JOIN knowledge_versions v ON v.knowledge_id = k.id AND v.is_latest = true
    WHERE 
        (p_category IS NULL OR k.category = p_category)
        AND (p_entry_type IS NULL OR k.entry_type = p_entry_type)
    ORDER BY k.updated_at DESC
    LIMIT p_limit;
END;
$$;

-- ============================================================================
-- EXECUTOR CHAIN RESOLUTION
-- ============================================================================

CREATE OR REPLACE FUNCTION resolve_executor_chain(
    p_tool_id text,
    p_max_depth integer DEFAULT 10
)
RETURNS TABLE(
    depth integer,
    tool_id text,
    version text,
    tool_type text,
    executor_id text,
    manifest jsonb,
    content_hash text,
    file_hashes jsonb
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
BEGIN
    RETURN QUERY
    WITH RECURSIVE chain AS (
        SELECT 
            0 AS depth,
            t.tool_id,
            tv.version,
            t.tool_type,
            t.executor_id,
            tv.manifest,
            tv.content_hash,
            (
                SELECT COALESCE(jsonb_agg(jsonb_build_object(
                    'path', tvf.path,
                    'sha256', tvf.sha256,
                    'is_executable', tvf.is_executable
                ) ORDER BY tvf.path), '[]'::jsonb)
                FROM tool_version_files tvf
                WHERE tvf.tool_version_id = tv.id
            ) AS file_hashes
        FROM tools t
        LEFT JOIN tool_versions tv ON tv.tool_id = t.id AND tv.is_latest = true
        WHERE t.tool_id = p_tool_id
        
        UNION ALL
        
        SELECT 
            c.depth + 1,
            t.tool_id,
            tv.version,
            t.tool_type,
            t.executor_id,
            tv.manifest,
            tv.content_hash,
            (
                SELECT COALESCE(jsonb_agg(jsonb_build_object(
                    'path', tvf.path,
                    'sha256', tvf.sha256,
                    'is_executable', tvf.is_executable
                ) ORDER BY tvf.path), '[]'::jsonb)
                FROM tool_version_files tvf
                WHERE tvf.tool_version_id = tv.id
            ) AS file_hashes
        FROM chain c
        JOIN tools t ON t.tool_id = c.executor_id
        LEFT JOIN tool_versions tv ON tv.tool_id = t.id AND tv.is_latest = true
        WHERE c.executor_id IS NOT NULL
          AND c.depth < p_max_depth
    )
    SELECT * FROM chain
    ORDER BY chain.depth;
END;
$$;

-- ============================================================================
-- USER TRUST SCORE
-- ============================================================================

-- Calculate trust score from user stats (on-demand, not stored)
CREATE OR REPLACE FUNCTION get_user_trust_score(p_user_id uuid)
RETURNS integer
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
DECLARE
    v_stats user_stats%ROWTYPE;
    v_score integer := 0;
BEGIN
    SELECT * INTO v_stats FROM user_stats WHERE user_id = p_user_id;
    
    IF NOT FOUND THEN
        RETURN 0;
    END IF;
    
    -- Score components (max 100):
    -- - Executions: up to 20 points (1 point per 100 executions, max 2000)
    -- - Published items: up to 30 points (3 points per item, max 10)
    -- - Downloads received: up to 30 points (1 point per 50 downloads, max 1500)
    -- - Streak: up to 10 points (1 point per 5 days, max 50)
    -- - Tenure: up to 10 points (1 point per 30 days, max 300 days)
    
    v_score := v_score + LEAST(20, v_stats.total_executions / 100);
    v_score := v_score + LEAST(30, v_stats.total_published * 3);
    v_score := v_score + LEAST(30, v_stats.total_downloads / 50);
    v_score := v_score + LEAST(10, v_stats.streak_days / 5);
    v_score := v_score + LEAST(10, EXTRACT(DAY FROM now() - v_stats.member_since) / 30);
    
    RETURN v_score;
END;
$$;

-- Get user with calculated trust score
CREATE OR REPLACE FUNCTION get_user_with_trust(p_user_id uuid)
RETURNS TABLE(
    id uuid,
    username text,
    email text,
    trust_score integer,
    created_at timestamptz
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
BEGIN
    RETURN QUERY
    SELECT 
        u.id,
        u.username,
        u.email,
        get_user_trust_score(u.id) as trust_score,
        u.created_at
    FROM users u
    WHERE u.id = p_user_id;
END;
$$;

-- ============================================================================
-- TELEMETRY SYNC FUNCTIONS
-- ============================================================================

-- Update user stats on telemetry sync
CREATE OR REPLACE FUNCTION update_user_stats(
    p_user_id uuid,
    p_executions integer
)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
DECLARE
    v_last_active date;
    v_today date := CURRENT_DATE;
BEGIN
    SELECT last_active::date INTO v_last_active
    FROM user_stats WHERE user_id = p_user_id;
    
    INSERT INTO user_stats (user_id, total_executions, last_active, streak_days)
    VALUES (p_user_id, p_executions, now(), 1)
    ON CONFLICT (user_id) DO UPDATE SET
        total_executions = user_stats.total_executions + p_executions,
        last_active = now(),
        streak_days = CASE
            WHEN v_last_active = v_today - 1 THEN user_stats.streak_days + 1
            WHEN v_last_active = v_today THEN user_stats.streak_days
            ELSE 1
        END;
END;
$$;

-- Upsert user activity by item type
CREATE OR REPLACE FUNCTION upsert_user_activity(
    p_user_id uuid,
    p_item_type text,
    p_executions integer,
    p_successes integer
)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
BEGIN
    INSERT INTO user_activity (user_id, item_type, executions, successes)
    VALUES (p_user_id, p_item_type, p_executions, p_successes)
    ON CONFLICT (user_id, item_type) DO UPDATE SET
        executions = user_activity.executions + p_executions,
        successes = user_activity.successes + p_successes;
END;
$$;

-- Increment execution count for an item
CREATE OR REPLACE FUNCTION increment_execution_count(
    p_item_type text,
    p_item_id text,
    p_count integer DEFAULT 1
)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
BEGIN
    IF p_item_type = 'directive' THEN
        UPDATE directives
        SET execution_count = execution_count + p_count
        WHERE name = p_item_id;
    ELSIF p_item_type = 'tool' THEN
        UPDATE tools
        SET execution_count = execution_count + p_count
        WHERE tool_id = p_item_id;
    ELSIF p_item_type = 'knowledge' THEN
        UPDATE knowledge
        SET execution_count = execution_count + p_count
        WHERE zettel_id = p_item_id;
    END IF;
END;
$$;

-- Batch increment execution counts (for sync)
CREATE OR REPLACE FUNCTION batch_increment_executions(
    p_items jsonb  -- [{item_type, item_id, count}, ...]
)
RETURNS integer
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
DECLARE
    v_item jsonb;
    v_count integer := 0;
BEGIN
    FOR v_item IN SELECT * FROM jsonb_array_elements(p_items)
    LOOP
        PERFORM increment_execution_count(
            v_item->>'item_type',
            v_item->>'item_id',
            COALESCE((v_item->>'count')::integer, 1)
        );
        v_count := v_count + 1;
    END LOOP;
    RETURN v_count;
END;
$$;

-- ============================================================================
-- TRIGGERS
-- ============================================================================

CREATE TRIGGER update_users_updated_at
    BEFORE UPDATE ON users
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();

CREATE TRIGGER update_directives_updated_at
    BEFORE UPDATE ON directives
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();

CREATE TRIGGER update_tools_updated_at
    BEFORE UPDATE ON tools
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();

CREATE TRIGGER update_knowledge_updated_at
    BEFORE UPDATE ON knowledge
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();

CREATE TRIGGER update_knowledge_search
    BEFORE INSERT OR UPDATE ON knowledge
    FOR EACH ROW EXECUTE FUNCTION update_knowledge_search_vector();

CREATE TRIGGER update_bundles_updated_at
    BEFORE UPDATE ON bundles
    FOR EACH ROW EXECUTE FUNCTION update_updated_at();
