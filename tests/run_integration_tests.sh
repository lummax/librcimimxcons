#!/bin/bash

library=`basename -s .so target/librcimmixcons-*.so | sed 's/lib//'`

function run_test {
    local file=$1;
    clang "tests/$file.c" -L target -l "$library" -o "target/$file" || return 1;
    LD_LIBRARY_PATH=target valgrind "./target/$file" || return 3;
    return 0;
}

code=0;
for path in tests/*.c; do
    file=`basename -s .c "$path"`;
    echo -n "Running test $file.."
    output=$(run_test $file 2>&1);
    if [ $? -ne 0 ]; then
        echo "fail";
        echo -e "\n$output\n" | tee "target/log_$file";
        code=1;
    else
        echo "ok";
    fi;
done;
exit $code;
