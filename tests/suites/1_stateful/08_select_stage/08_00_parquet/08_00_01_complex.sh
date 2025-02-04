#!/usr/bin/env bash

CURDIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$CURDIR"/../../../../shell_env.sh

run() {
		echo "--- $1"
		echo "$1" | $MYSQL_CLIENT_CONNECT
}

for USE_PARQUET2 in 0 1; do

echo "USE_PARQUET2=${USE_PARQUET2}"

run "drop stage if exists s1;"
run "create stage s1 FILE_FORMAT = (type = PARQUET);"

aws --endpoint-url http://127.0.0.1:9900/ s3 cp s3://testbucket/admin/data/tuple.parquet s3://testbucket/admin/stage/internal/s1/tuple.parquet  >/dev/null 2>&1

run "set use_parquet2 = ${USE_PARQUET2} ; select * from @s1;"

run "set use_parquet2 = ${USE_PARQUET2} ; select * from @s1 where t:a = 1;"

run "set use_parquet2 = ${USE_PARQUET2} ; select * from @s1 where t:a = id;"

run "set use_parquet2 = ${USE_PARQUET2} ; select * from @s1 where t:a >= 2;"

run "set use_parquet2 = ${USE_PARQUET2} ; select t:b from @s1 where t:a >= 2;"

run "set use_parquet2 = ${USE_PARQUET2} ; select t:b from @s1;"

run "set use_parquet2 = ${USE_PARQUET2} ; select t from @s1;"

run "set use_parquet2 = ${USE_PARQUET2} ; select id, t:a, t:b, t from @s1;"

run "set use_parquet2 = ${USE_PARQUET2} ; select id, t:a, t:b, t from @s1 where id > 2;"

run "set use_parquet2 = ${USE_PARQUET2} ; select id, t:a, t:b, t from @s1 where t:b < 'c';"

run "set use_parquet2 = ${USE_PARQUET2} ; select * from @s1 where t:b < 'c';"

run "set use_parquet2 = ${USE_PARQUET2} ; select t:a from @s1 where t:b < 'c';"

run "set use_parquet2 = ${USE_PARQUET2} ; select id from @s1 where t:b < 'c';"


run "drop stage if exists s2;"
run "create stage s2 FILE_FORMAT = (type = PARQUET);"

aws --endpoint-url http://127.0.0.1:9900/ s3 cp s3://testbucket/admin/data/complex.parquet s3://testbucket/admin/stage/internal/s2/complex.parquet  >/dev/null 2>&1

run "set use_parquet2 = ${USE_PARQUET2} ; select meta from @s2 limit 3;"

run "set use_parquet2 = ${USE_PARQUET2} ; select meta.2, meta.6 from @s2 limit 3;"

run "set use_parquet2 = ${USE_PARQUET2} ; select name from @s2 limit 3;"

run "set use_parquet2 = ${USE_PARQUET2} ; select name[1] from @s2 limit 3;"

run "set use_parquet2 = ${USE_PARQUET2} ; select name[1].5 from @s2 limit 3;"

run "set use_parquet2 = ${USE_PARQUET2} ; select name[2] from @s2 limit 3;"

run "set use_parquet2 = ${USE_PARQUET2} ; select name[2].6 from @s2 limit 3;"

done