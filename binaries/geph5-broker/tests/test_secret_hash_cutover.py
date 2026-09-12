"""Exercise the exact startup SQL scripts on fresh databases in loopback PostgreSQL.

GEPH_TEST_DATABASE_URL=postgres://...@127.0.0.1:55439/postgres \
    python3 tests/test_secret_hash_cutover.py
Set GEPH_HASH_VOLUME_TEST=1 to include a 3.55-million-account backfill.
Only generated databases are changed; each is dropped after its test.
"""

import contextlib
import hashlib
import os
from pathlib import Path
import subprocess
import time
import unittest
from urllib.parse import urlsplit, urlunsplit
import uuid


ROOT = Path(__file__).resolve().parents[1]
CREATE = ROOT / "sql/auth_secret_hash_01_create.sql"
BACKFILL = ROOT / "sql/auth_secret_hash_02_backfill.sql"
TOKEN_CREATE = ROOT / "sql/auth_token_hash_01_create.sql"
TOKEN_BACKFILL = ROOT / "sql/auth_token_hash_02_backfill.sql"
ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"


def invite(secret):
    value = int.from_bytes(hashlib.sha256(("invite-code" + secret).encode()).digest()[:10], "big")
    return "".join(ALPHABET[(value >> shift) & 31] for shift in range(75, -1, -5))


class Cutover(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.admin_url = os.environ.get("GEPH_TEST_DATABASE_URL")
        if not cls.admin_url:
            raise unittest.SkipTest("set GEPH_TEST_DATABASE_URL to disposable loopback PostgreSQL")
        cls.parts = urlsplit(cls.admin_url)
        if cls.parts.hostname not in ("127.0.0.1", "localhost", "::1"):
            raise RuntimeError("refusing to run against a non-loopback database")

    def psql(self, sql=None, path=None, *, admin=False, check=True, extra_env=None):
        cmd = ["psql", self.admin_url if admin else self.url,
               "-X", "-qAt", "-v", "ON_ERROR_STOP=1"]
        if path:
            cmd += ["-f", str(path)]
        env = dict(os.environ, PGCONNECT_TIMEOUT="5")
        env.pop("PGOPTIONS", None)
        env.update(extra_env or {})
        result = subprocess.run(cmd, input=sql, text=True, capture_output=True, env=env, timeout=360)
        if check and result.returncode:
            self.fail(result.stderr)
        return result

    def scalar(self, sql):
        return self.psql(sql).stdout.strip()

    def setUp(self):
        self.name = "geph_hash_test_" + uuid.uuid4().hex
        self.url = urlunsplit(self.parts._replace(path="/" + self.name))
        self.psql("CREATE DATABASE " + self.name, admin=True)
        self.addCleanup(lambda: self.psql("DROP DATABASE " + self.name + " WITH (FORCE)", admin=True))
        self.psql(path=ROOT / "tests/secret_hash_fixture.sql")

    def test_full_cutover(self):
        self.psql("SELECT addplusbysecret('000123', interval '1 day')")
        self.psql(path=CREATE)
        self.psql(path=BACKFILL)
        self.assertEqual(self.scalar("SELECT to_regclass('public.auth_secret') IS NULL"), "t")
        self.assertEqual(self.scalar("SELECT count(*) FROM public.auth_secret_hash"), "3")
        for account_id, secret in [(1, "900000000000000000000001"), (2, "000123"), (3, "UTF8-密钥")]:
            digest = self.scalar(f"SELECT encode(secret_hash, 'hex') FROM auth_secret_hash WHERE id={account_id}")
            self.assertEqual(digest, hashlib.sha256(secret.encode()).hexdigest())
        self.assertEqual(self.scalar("SELECT code FROM invite_codes WHERE user_id=1"), invite("900000000000000000000001"))
        self.assertEqual(self.scalar("SELECT code FROM invite_codes WHERE user_id=2"), "EXISTING-CODE")
        self.assertEqual(self.scalar("SELECT code FROM invite_codes WHERE user_id=3"), invite("UTF8-密钥"))
        self.assertEqual(self.scalar("SELECT pwdhash FROM auth_password"), "unchanged-password-hash")
        self.assertEqual(self.scalar("SELECT token FROM auth_tokens"), "unchanged-token")
        self.assertEqual(self.scalar("SELECT version || ':' || encode(checksum, 'hex') FROM _sqlx_migrations"), "123:abcd")
        self.psql("SELECT addplusbysecret('000123', interval '2 days'); SELECT addplusbysecret('000123', interval '1 day', 0)")
        self.assertEqual(self.scalar("SELECT count(*) FROM showplusbysecret('000123')"), "3")
        self.assertEqual(self.scalar("SELECT sum(extract(epoch FROM end_time-start_time))::integer FROM showplusbysecret('000123') WHERE tier=1"), str(3 * 86400))
        self.assertEqual(self.scalar("SELECT count(*) FROM showplusbysecret('123')"), "0")
        self.psql("INSERT INTO revenue SELECT period_id, 500, 'test', '{}'::jsonb FROM plus_periods WHERE tier=0")
        self.assertEqual(self.scalar("SELECT eurocents || ':' || method FROM showrevenuebysecret('000123')"), "500:test")
        self.psql("DELETE FROM invite_codes WHERE user_id=3; DELETE FROM users WHERE id=3")
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_secret_hash WHERE id=3"), "0")

    def test_failed_cutover_rolls_back_and_can_retry(self):
        self.psql(path=CREATE)
        self.psql("CREATE VIEW secret_dependency AS SELECT id FROM auth_secret")
        result = self.psql(path=BACKFILL, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("depend", result.stderr)
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_secret"), "3")
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_secret_hash"), "0")
        self.assertEqual(self.scalar("SELECT count(*) FROM invite_codes"), "1")
        self.assertEqual(self.scalar("SELECT prosrc LIKE '%s.secret = p_secret%' FROM pg_proc WHERE oid='showplusbysecret(text)'::regprocedure"), "t")
        self.psql("DROP VIEW secret_dependency")
        self.psql(path=BACKFILL)
        self.assertEqual(self.scalar("SELECT to_regclass('auth_secret') IS NULL"), "t")

    def test_mismatched_existing_hash_prevents_plaintext_removal(self):
        self.psql(path=CREATE)
        self.psql("INSERT INTO auth_secret_hash VALUES (1, sha256('wrong'::bytea))")
        result = self.psql(path=BACKFILL, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("verification failed", result.stderr)
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_secret"), "3")
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_secret_hash"), "1")
        self.psql("DELETE FROM auth_secret_hash")
        self.psql(path=BACKFILL)

    def test_original_table_grants_are_preserved(self):
        role = "hash_support_" + uuid.uuid4().hex
        self.psql("CREATE ROLE " + role, admin=True)

        def remove_role():
            self.psql("DROP OWNED BY " + role)
            self.psql("DROP ROLE " + role, admin=True)

        self.addCleanup(remove_role)
        self.psql(f"GRANT SELECT ON public.auth_secret TO {role} WITH GRANT OPTION")
        self.psql(path=CREATE)
        self.psql(path=BACKFILL)
        self.assertEqual(self.scalar(f"SELECT has_table_privilege('{role}', 'auth_secret_hash', 'SELECT WITH GRANT OPTION')"), "t")
        self.assertEqual(self.scalar(f"SET ROLE {role}; SELECT count(*) FROM public.auth_secret_hash"), "3")

    @contextlib.contextmanager
    def hold_lock(self, table, mode):
        app = "hash_lock_" + uuid.uuid4().hex
        cmd = ["psql", self.url, "-X", "-qAt", "-v", "ON_ERROR_STOP=1", "-c",
               f"BEGIN; LOCK TABLE {table} IN {mode} MODE; SELECT pg_sleep(30); ROLLBACK;"]
        proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                env=dict(os.environ, PGAPPNAME=app))
        try:
            deadline = time.monotonic() + 5
            while self.scalar(f"SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a USING(pid) WHERE a.application_name='{app}' AND l.relation='{table}'::regclass AND l.granted") == "0":
                if proc.poll() is not None or time.monotonic() > deadline:
                    self.fail("lock holder failed to acquire its lock")
                time.sleep(0.02)
            yield
        finally:
            self.psql(f"SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE application_name='{app}'")
            proc.wait(timeout=5)

    def test_schema_lock_wait_is_bounded(self):
        with self.hold_lock("public.users", "ROW EXCLUSIVE"):
            start = time.monotonic()
            result = self.psql(path=CREATE, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("lock timeout", result.stderr)
            self.assertLess(time.monotonic() - start, 3)
        self.assertEqual(self.scalar("SELECT to_regclass('auth_secret_hash') IS NULL"), "t")
        self.psql(path=CREATE)
        self.psql(path=BACKFILL)

    def test_final_drop_lock_timeout_keeps_plaintext(self):
        self.psql(path=CREATE)
        # An ordinary reader permits the backfill but prevents the final DROP.
        with self.hold_lock("public.auth_secret", "ACCESS SHARE"):
            result = self.psql(path=BACKFILL, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("lock timeout", result.stderr)
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_secret_hash"), "0")
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_secret"), "3")
        self.psql(path=BACKFILL)

    def test_device_token_cutover_preserves_exact_tokens_and_user_mappings(self):
        self.psql("""
            INSERT INTO auth_tokens VALUES ('000123', 1), ('00123', 2),
                ('UTF8-密钥', 3), ('caseSensitive', 1), ('CASESENSITIVE', 2),
                ('orphan-token', 999999);
        """)
        self.psql(path=TOKEN_CREATE)
        self.psql(path=TOKEN_BACKFILL)
        self.assertEqual(self.scalar("SELECT to_regclass('auth_tokens') IS NULL"), "t")
        tokens = [('unchanged-token', 1), ('000123', 1), ('00123', 2),
                  ('UTF8-密钥', 3), ('caseSensitive', 1), ('CASESENSITIVE', 2),
                  ('orphan-token', 999999)]
        for token, user_id in tokens:
            digest = hashlib.sha256(token.encode()).hexdigest()
            self.assertEqual(self.scalar(f"SELECT user_id FROM auth_token_hash WHERE token_hash=decode('{digest}', 'hex')"), str(user_id))
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_token_hash"), str(len(tokens)))
        self.assertEqual(self.scalar("SELECT count(*) FROM pg_indexes WHERE tablename='auth_token_hash' AND indexdef LIKE '%(user_id)%'"), "1")
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_secret"), "3")
        invalid = self.psql("INSERT INTO auth_token_hash VALUES ('short'::bytea, 1)", check=False)
        self.assertNotEqual(invalid.returncode, 0)

    def test_device_token_grants_are_preserved(self):
        role = "token_support_" + uuid.uuid4().hex
        self.psql("CREATE ROLE " + role, admin=True)

        def remove_role():
            self.psql("DROP OWNED BY " + role)
            self.psql("DROP ROLE " + role, admin=True)

        self.addCleanup(remove_role)
        self.psql(f"GRANT SELECT ON public.auth_tokens TO {role} WITH GRANT OPTION")
        self.psql(path=TOKEN_CREATE)
        self.psql(path=TOKEN_BACKFILL)
        self.assertEqual(self.scalar(f"SELECT has_table_privilege('{role}', 'auth_token_hash', 'SELECT WITH GRANT OPTION')"), "t")
        self.assertEqual(self.scalar(f"SET ROLE {role}; SELECT count(*) FROM auth_token_hash"), "1")

    @unittest.skipUnless(os.environ.get("GEPH_TOKEN_HASH_VOLUME_TEST") == "1", "opt-in token volume test")
    def test_device_token_representative_volume(self):
        self.psql("""
            INSERT INTO auth_tokens
            SELECT substr(md5(g::text), 1, 30), (g % 3550000) + 1
            FROM generate_series(1, 12000000) g;
            ANALYZE auth_tokens;
        """)
        self.psql(path=TOKEN_CREATE)
        start = time.monotonic()
        self.psql(path=TOKEN_BACKFILL)
        elapsed = time.monotonic() - start
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_token_hash"), "12000001")
        print(f"\n12,000,001-token backfill completed in {elapsed:.1f}s", flush=True)

    @unittest.skipUnless(os.environ.get("GEPH_HASH_VOLUME_TEST") == "1", "opt-in volume test")
    def test_representative_volume(self):
        self.psql("""
            INSERT INTO users (id) SELECT g FROM generate_series(4,3550000) g;
            INSERT INTO auth_secret SELECT g, lpad(g::text, 24, '0') FROM generate_series(4,3550000) g;
            INSERT INTO invite_codes SELECT g, 'existing-' || g FROM generate_series(4,3550000) g WHERE g % 20 <> 0;
            ANALYZE users; ANALYZE auth_secret; ANALYZE invite_codes;
        """)
        self.psql(path=CREATE)
        start = time.monotonic()
        self.psql(path=BACKFILL)
        elapsed = time.monotonic() - start
        self.assertEqual(self.scalar("SELECT count(*) FROM auth_secret_hash"), "3550000")
        self.assertEqual(self.scalar("SELECT count(*) FROM invite_codes"), "3550000")
        print(f"\n3,550,000-account backfill completed in {elapsed:.1f}s", flush=True)


if __name__ == "__main__":
    unittest.main(verbosity=2)
