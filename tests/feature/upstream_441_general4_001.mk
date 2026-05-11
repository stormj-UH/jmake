# Make sure that subdirectories built as prerequisites are actually handled
# properly.

all: dir/subdir/file.a

dir/subdir: ; @echo mkdir -p dir/subdir

dir/subdir/file.b: dir/subdir ; @echo touch dir/subdir/file.b

dir/subdir/%.a: dir/subdir/%.b ; @echo cp $< $@