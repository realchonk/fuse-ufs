#!/bin/sh -e

sed 's@(#\([0-9]*\))@([#\1](https://github.com/realchonk/fuse-ufs/pull/\1))@'	\
	< ChangeLog.md > ChangeLog.new
mv ChangeLog.new ChangeLog.md
