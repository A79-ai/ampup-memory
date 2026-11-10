# Local dev loop: the harness stack (Polaris + SeaweedFS in Docker), the engine built on every
# source change, a corpus loaded into one scope and `memstore serve` with MCP at /mcp.
#
#   tilt up                                   # an empty memory: no corpus is loaded
#   tilt up -- --corpus ~/my-corpus           # a corpus dir (episodes.jsonl, facts.jsonl)
#   tilt down                                 # stops the stack; it keeps no volumes, so data goes too
#
# Locally there is no caller identity: every request reads as group --group, so serve stays on
# 127.0.0.1.

config.define_string('corpus', usage='corpus dir with episodes.jsonl and facts.jsonl; default: none, nothing is loaded')
config.define_string('scope', usage='scope to index and serve (default org)')
config.define_string('port', usage='serve port (default 8080)')
config.define_string('group', usage='group every request reads as (default local)')
cfg = config.parse()

scope = cfg.get('scope', 'org')
port = cfg.get('port', '8080')
group = cfg.get('group', 'local')
corpus = cfg.get('corpus', '')

state = 'corpora/tilt'
B = './target/release/ampup-memory'
# harness/env.sh points the engine at the stack; the index lives apart from ~/.cache/memstore/index.
engine = '. harness/env.sh && export AMPUP_MEMORY_INDEX_URI=$PWD/%s/index && ' % state
os.putenv('PATH', os.getenv('HOME') + '/.cargo/bin:' + os.getenv('PATH'))

docker_compose('harness/docker-compose.yml')
for svc in ['s3', 's3-init', 'polaris', 'polaris-init']:
    dc_resource(svc, labels=['stack'])

local_resource(
    'build',
    'cargo build --release',
    deps=['src', 'schemas', 'Cargo.toml', 'Cargo.lock'],
    labels=['engine'],
)

local_resource(
    'smoke',
    engine + B + ' smoke user:tilt',
    resource_deps=['build', 'polaris-init'],
    trigger_mode=TRIGGER_MODE_MANUAL,
    labels=['engine'],
)

# No corpus, nothing is loaded: a dev memory never fills with synthetic data by default (a synthetic one is
# `memstore gen 42 1000 <dir>`, passed with --corpus).
if corpus:
    load_cmd = (engine + 'set -e; C=%s; rm -rf $AMPUP_MEMORY_INDEX_URI; ' % corpus +
            B + ' load $C; ' + B + ' index ' + scope)
else:
    load_cmd = 'echo "no corpus given (tilt up -- --corpus <dir>): nothing loaded"'

# The stack keeps no volumes, so after the stack restarts trigger `load` again (the index is
# rebuilt with it).
local_resource(
    'load',
    load_cmd,
    resource_deps=['build', 'polaris-init'],
    trigger_mode=TRIGGER_MODE_MANUAL,
    labels=['data'],
)

local_resource(
    'serve',
    serve_cmd=engine + 'exec ' + B + ' serve ' + scope + ' ' + port + ' ' + group,
    resource_deps=['load'],
    deps=['target/release/ampup-memory'],
    readiness_probe=probe(tcp_socket=tcp_socket_action(int(port))),
    links=[link('http://localhost:%s/mcp' % port, 'MCP')],
    labels=['engine'],
)
