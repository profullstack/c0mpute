-- Quest schema, v0. See PRD §13.
--
-- Conventions:
--   * UUID PKs everywhere except auth.users (Supabase manages that one).
--   * Append-only ledger tables (`earnings`, `billing`) — never UPDATE rows;
--     reverse a transaction by inserting a compensating row.
--   * Soft delete via `status='deleted'` rather than DELETE so chunk
--     retention/garbage-collection can be reasoned about.
--   * RLS is on for every user-owned table; coordinator uses service role
--     and bypasses RLS for its own queries.

create extension if not exists "pgcrypto";

-- ────────────────────────────────────────────────────────────────────────
-- Identity
-- ────────────────────────────────────────────────────────────────────────

create table if not exists profiles (
  id uuid primary key references auth.users(id) on delete cascade,
  display_name text,
  role text not null default 'customer'
    check (role in ('customer','provider','both','operator')),
  coinpayments_merchant_id text,
  payout_wallet text,
  created_at timestamptz not null default now()
);

-- ────────────────────────────────────────────────────────────────────────
-- Content
-- ────────────────────────────────────────────────────────────────────────

create table if not exists videos (
  id uuid primary key default gen_random_uuid(),
  owner_id uuid not null references profiles(id) on delete cascade,
  title text not null,
  source_size_bytes bigint,
  duration_seconds int,
  root_manifest_hash text,
  status text not null default 'uploading'
    check (status in ('uploading','queued','transcoding','ready','failed','deleted')),
  renditions jsonb default '[]'::jsonb,
  created_at timestamptz not null default now(),
  ready_at timestamptz
);

create index if not exists videos_owner_idx on videos(owner_id);
create index if not exists videos_status_idx on videos(status);

create table if not exists renditions (
  id uuid primary key default gen_random_uuid(),
  video_id uuid not null references videos(id) on delete cascade,
  name text not null,
  codec text not null,
  resolution text not null,
  bitrate_bps int not null,
  manifest_hash text,
  total_chunks int default 0,
  total_bytes bigint default 0,
  created_at timestamptz not null default now()
);

create index if not exists renditions_video_idx on renditions(video_id);

create table if not exists shard_sets (
  id uuid primary key default gen_random_uuid(),
  k int not null,
  n int not null,
  shards jsonb not null default '[]'::jsonb
);

create table if not exists chunks (
  id uuid primary key default gen_random_uuid(),
  rendition_id uuid not null references renditions(id) on delete cascade,
  sequence_no int not null,
  chunk_hash text unique not null,
  bytes int not null,
  duration_seconds float,
  is_keyframe_aligned boolean default false,
  shard_set_id uuid references shard_sets(id) on delete set null
);

create index if not exists chunks_rendition_seq_idx on chunks(rendition_id, sequence_no);

-- ────────────────────────────────────────────────────────────────────────
-- Providers
-- ────────────────────────────────────────────────────────────────────────

create table if not exists providers (
  id uuid primary key default gen_random_uuid(),
  owner_id uuid references profiles(id) on delete set null,
  peer_id text unique not null,
  hardware jsonb default '{}'::jsonb,
  capabilities jsonb default '{}'::jsonb,
  reputation_score double precision default 0.5
    check (reputation_score >= 0 and reputation_score <= 1),
  status text not null default 'offline'
    check (status in ('online','offline','suspended','slashed')),
  stake_amount numeric default 0,
  last_heartbeat timestamptz,
  created_at timestamptz not null default now()
);

create index if not exists providers_owner_idx on providers(owner_id);
create index if not exists providers_status_idx on providers(status);

-- ────────────────────────────────────────────────────────────────────────
-- Jobs
-- ────────────────────────────────────────────────────────────────────────

create table if not exists jobs (
  id uuid primary key default gen_random_uuid(),
  video_id uuid references videos(id) on delete cascade,
  rendition_id uuid references renditions(id) on delete cascade,
  chunk_sequence int,
  assigned_to uuid references providers(id) on delete set null,
  status text not null default 'queued'
    check (status in ('queued','running','completed','failed','expired')),
  spec jsonb not null,
  result_hash text,
  error text,
  created_at timestamptz not null default now(),
  claimed_at timestamptz,
  completed_at timestamptz,
  payout_amount_usd numeric
);

create index if not exists jobs_status_idx on jobs(status);
create index if not exists jobs_provider_idx on jobs(assigned_to);

-- Atomic job claim. Picks the oldest queued job, marks it running, returns
-- it to the worker. SKIP LOCKED keeps multiple workers from racing.
create or replace function claim_next_job(p_provider uuid)
returns jsonb
language plpgsql
as $$
declare
  v_row jobs%rowtype;
begin
  select *
    into v_row
    from jobs
    where status = 'queued'
    order by created_at
    limit 1
    for update skip locked;

  if not found then
    return null;
  end if;

  update jobs
     set status = 'running',
         assigned_to = p_provider,
         claimed_at = now()
   where id = v_row.id;

  return jsonb_build_object(
    'job_id', v_row.id,
    'video_id', v_row.video_id,
    'rendition_id', v_row.rendition_id,
    'spec', v_row.spec
  );
end
$$;

-- ────────────────────────────────────────────────────────────────────────
-- Verification
-- ────────────────────────────────────────────────────────────────────────

create table if not exists challenges (
  id uuid primary key default gen_random_uuid(),
  provider_id uuid not null references providers(id) on delete cascade,
  target_chunk_hash text not null,
  challenge_offset int not null,
  challenge_length int not null,
  expected_response_hash text not null,
  status text not null default 'issued'
    check (status in ('issued','passed','failed','expired')),
  issued_at timestamptz not null default now(),
  responded_at timestamptz
);

create index if not exists challenges_provider_idx on challenges(provider_id);

-- ────────────────────────────────────────────────────────────────────────
-- Money (append-only)
-- ────────────────────────────────────────────────────────────────────────

create table if not exists earnings (
  id uuid primary key default gen_random_uuid(),
  provider_id uuid not null references providers(id),
  job_id uuid references jobs(id),
  type text not null
    check (type in ('transcode','storage','egress','gateway','bonus','slash','withdraw')),
  amount_usd numeric not null,
  amount_stable numeric,
  stablecoin text,
  created_at timestamptz not null default now()
);

create index if not exists earnings_provider_idx on earnings(provider_id, created_at desc);

create table if not exists billing (
  id uuid primary key default gen_random_uuid(),
  customer_id uuid not null references profiles(id),
  type text not null
    check (type in ('topup','transcode','storage','egress','refund')),
  amount_usd numeric not null,
  reference jsonb default '{}'::jsonb,
  coinpayments_tx text,
  created_at timestamptz not null default now()
);

create index if not exists billing_customer_idx on billing(customer_id, created_at desc);

-- ────────────────────────────────────────────────────────────────────────
-- Row-level security
-- ────────────────────────────────────────────────────────────────────────

alter table profiles enable row level security;
alter table videos enable row level security;
alter table renditions enable row level security;
alter table chunks enable row level security;
alter table providers enable row level security;
alter table jobs enable row level security;
alter table challenges enable row level security;
alter table earnings enable row level security;
alter table billing enable row level security;

-- Profiles: users see their own row.
create policy profiles_self on profiles
  for select using (auth.uid() = id);
create policy profiles_self_update on profiles
  for update using (auth.uid() = id);

-- Videos: customers see their own. Operators see all.
create policy videos_owner on videos
  for select using (auth.uid() = owner_id);
create policy videos_insert on videos
  for insert with check (auth.uid() = owner_id);
create policy videos_update on videos
  for update using (auth.uid() = owner_id);

-- Renditions / chunks ride on the video's owner.
create policy renditions_via_video on renditions
  for select using (
    exists (select 1 from videos v where v.id = renditions.video_id and v.owner_id = auth.uid())
  );

create policy chunks_via_video on chunks
  for select using (
    exists (
      select 1
        from renditions r
        join videos v on v.id = r.video_id
       where r.id = chunks.rendition_id and v.owner_id = auth.uid()
    )
  );

-- Providers: operators see their own nodes.
create policy providers_self on providers
  for select using (auth.uid() = owner_id);
create policy providers_self_update on providers
  for update using (auth.uid() = owner_id);

-- Earnings: providers see their own.
create policy earnings_self on earnings
  for select using (
    exists (select 1 from providers p where p.id = earnings.provider_id and p.owner_id = auth.uid())
  );

-- Billing: customers see their own.
create policy billing_self on billing
  for select using (auth.uid() = customer_id);

-- Jobs: provider sees jobs they were assigned. Customer sees jobs for their
-- videos.
create policy jobs_provider on jobs
  for select using (
    exists (select 1 from providers p where p.id = jobs.assigned_to and p.owner_id = auth.uid())
  );
create policy jobs_customer on jobs
  for select using (
    exists (select 1 from videos v where v.id = jobs.video_id and v.owner_id = auth.uid())
  );

-- Challenges: providers see their own.
create policy challenges_self on challenges
  for select using (
    exists (select 1 from providers p where p.id = challenges.provider_id and p.owner_id = auth.uid())
  );
