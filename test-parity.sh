#!/bin/bash
# SwingMusix F1 — E2E auth parity test against a isolated test env (CT104).
# Does NOT touch CT101 data. Uses /tmp/smxtest config dir.
set -u
cd /opt/swingmusix
. $HOME/.cargo/env

CFG=/tmp/smxtest
PORT=1971
SERVER_ID=$(python3 -c "import secrets;print(secrets.token_hex(32))")
PW="testpassword123"

rm -rf $CFG; mkdir -p $CFG/swingmusic
cat > $CFG/swingmusic/settings.json <<EOF
{"serverId": "$SERVER_ID", "usersOnLogin": true, "rootDirs": ["/mnt/music"]}
EOF

python3 - "$CFG/swingmusic/swingmusic.db" "$SERVER_ID" "$PW" <<'PYEOF'
import hashlib, sqlite3, sys
db, sid, pw = sys.argv[1], sys.argv[2], sys.argv[3]
conn = sqlite3.connect(db)
conn.execute("CREATE TABLE user (id INTEGER NOT NULL, image VARCHAR, password VARCHAR NOT NULL, username VARCHAR NOT NULL, roles JSON NOT NULL, extra JSON, PRIMARY KEY (id))")
conn.execute("CREATE INDEX ix_user_username ON user (username)")
h = hashlib.pbkdf2_hmac("sha256", pw.encode(), sid.encode(), 100000).hex()
conn.execute("INSERT INTO user (username, password, roles) VALUES (?,?,?)", ("admin", h, '["admin"]'))
conn.execute("INSERT INTO user (username, password, roles) VALUES (?,?,?)", ("guest", h, '["guest"]'))
conn.commit()
print("db ready:", db)
PYEOF

SWINGMUSIC_CONFIG_DIR=$CFG SWINGMUSIC_PORT=$PORT ./target/release/swingmusix > /tmp/smx-server.log 2>&1 &
SRV=$!
sleep 1.5

B="http://127.0.0.1:$PORT"
pass=0; fail=0
check() { # name expected actual
  if [ "$2" = "$3" ]; then echo "PASS: $1"; pass=$((pass+1));
  else echo "FAIL: $1 — expected [$2] got [$3]"; fail=$((fail+1)); fi
}

# 1. login OK → 200 + accesstoken + cookie
R=$(curl -s -D /tmp/h1 -o /tmp/b1 -w "%{http_code}" -X POST "$B/auth/login" -H 'Content-Type: application/json' -d "{\"username\":\"admin\",\"password\":\"$PW\"}")
check "login OK status" "200" "$R"
check "login msg" "Logged in as admin" "$(python3 -c "import json;print(json.load(open('/tmp/b1'))['msg'])")"
grep -q '"accesstoken"' /tmp/b1 && { echo "PASS: accesstoken present"; pass=$((pass+1)); } || { echo "FAIL: accesstoken missing"; fail=$((fail+1)); }
grep -q 'access_token_cookie=' /tmp/h1 && { echo "PASS: Set-Cookie access_token_cookie"; pass=$((pass+1)); } || { echo "FAIL: cookie missing"; fail=$((fail+1)); }
grep -q 'HttpOnly' /tmp/h1 && { echo "PASS: cookie HttpOnly"; pass=$((pass+1)); } || { echo "FAIL: HttpOnly missing"; fail=$((fail+1)); }
grep -q 'Max-Age=2592000' /tmp/h1 && { echo "PASS: cookie Max-Age 2592000"; pass=$((pass+1)); } || { echo "FAIL: Max-Age wrong"; fail=$((fail+1)); }

# 2. bad user → 404 {"msg":"User not found"}
R=$(curl -s -o /tmp/b2 -w "%{http_code}" -X POST "$B/auth/login" -H 'Content-Type: application/json' -d '{"username":"nobody","password":"x"}')
check "login bad user status" "404" "$R"
check "login bad user body" '{"msg":"User not found"}' "$(cat /tmp/b2)"

# 3. bad password → 401
R=$(curl -s -o /tmp/b3 -w "%{http_code}" -X POST "$B/auth/login" -H 'Content-Type: application/json' -d '{"username":"admin","password":"wrong"}')
check "login bad pw status" "401" "$R"
check "login bad pw body" '{"msg":"Hehe! invalid password"}' "$(cat /tmp/b3)"

# 4. token from cookie auth → /auth/user
TOKEN=$(python3 -c "import json;print(json.load(open('/tmp/b1'))['accesstoken'])")
R=$(curl -s -o /tmp/b4 -w "%{http_code}" "$B/auth/user" -H "Authorization: Bearer $TOKEN")
check "user w/ Bearer status" "200" "$R"
check "user body id" "1" "$(python3 -c "import json;print(json.load(open('/tmp/b4'))['id'])")"

# 5. no token → 401
R=$(curl -s -o /tmp/b5 -w "%{http_code}" "$B/auth/user")
check "user NOtok status" "401" "$R"

# 6. refresh w/ access token → 422
R=$(curl -s -o /tmp/b6 -w "%{http_code}" -X POST "$B/auth/refresh" -H "Authorization: Bearer $TOKEN")
check "refresh w/ access tok" "422" "$R"
check "refresh wrong-type msg" '{"msg":"Only refresh tokens are allowed"}' "$(cat /tmp/b6)"

# 7. user w/ refresh token → 422
RTOKEN=$(python3 -c "import json;print(json.load(open('/tmp/b1'))['refreshtoken'])")
R=$(curl -s -o /tmp/b7 -w "%{http_code}" "$B/auth/user" -H "Authorization: Bearer $RTOKEN")
check "user w/ refresh tok" "422" "$R"
check "user wrong-type msg" '{"msg":"Only non-refresh tokens are allowed"}' "$(cat /tmp/b7)"

# 8. refresh w/ refresh token → 200 (no Set-Cookie)
R=$(curl -s -D /tmp/h8 -o /tmp/b8 -w "%{http_code}" -X POST "$B/auth/refresh" -H "Authorization: Bearer $RTOKEN")
check "refresh OK status" "200" "$R"
check "refresh no Set-Cookie" "0" "$(grep -c 'Set-Cookie' /tmp/h8 || true)"

# 9. tampered token → 422 Signature verification failed
R=$(curl -s -o /tmp/b9 -w "%{http_code}" "$B/auth/user" -H "Authorization: Bearer ${TOKEN}x")
check "tampered tok status" "422" "$R"
check "tampered tok msg" '{"msg":"Signature verification failed"}' "$(cat /tmp/b9)"

# 10. logout → Max-Age=0
R=$(curl -s -D /tmp/h10 -o /tmp/b10 -w "%{http_code}" "$B/auth/logout")
check "logout status" "200" "$R"
grep -q 'Max-Age=0' /tmp/h10 && { echo "PASS: logout Max-Age=0"; pass=$((pass+1)); } || { echo "FAIL: logout cookie"; fail=$((fail+1)); }

# 11. unknown route NOtok → 401 (guard before routing)
R=$(curl -s -o /tmp/b11 -w "%{http_code}" "$B/zzz/nonexistent")
check "unknown route NOtok" "401" "$R"

# 12. /docs/openapi.json → 200 no token
R=$(curl -s -o /tmp/b12 -w "%{http_code}" "$B/docs/openapi.json")
check "docs openapi status" "200" "$R"

# 13. /auth/users anon → simplified users list
R=$(curl -s -o /tmp/b13 -w "%{http_code}" "$B/auth/users")
check "users anon status" "200" "$R"
python3 - <<'PYEOF'
import json
d = json.load(open('/tmp/b13'))
u = d['users'][0] if d['users'] else {}
assert set(u.keys()) == {'id','username','image','firstname'}, f"simplified keys wrong: {u.keys()}"
print("PASS: users anon simplified keys")
PYEOF

# 14. /auth/user body byte-parity vs live CT101 shape (alphabetical keys)
R=$(curl -s -o /tmp/b14 -w "%{http_code}" "$B/auth/user" -H "Authorization: Bearer $TOKEN")
check "user body byte-parity" '{"extra":null,"id":1,"image":null,"roles":["admin"],"username":"admin"}' "$(cat /tmp/b14)"
python3 -c "import sys;sys.exit(0 if open('/tmp/b14','rb').read().endswith(bytes([10])) else 1)" \
  && { echo "PASS: JSON body trailing newline (Flask jsonify parity)"; pass=$((pass+1)); } \
  || { echo "FAIL: JSON body missing trailing newline"; fail=$((fail+1)); }

kill $SRV 2>/dev/null
echo "=== RESULT: pass=$pass fail=$fail ==="
