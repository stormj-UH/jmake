# Negative
n = -10
# Zero
z = 0
# Positive
p = 888
min = -9223372036854775808
max = 9223372036854775807
huge = 8857889956778499040639527525992734031025567913257255490371761260681427
.RECIPEPREFIX = >
all:
> @echo 0_1 $(intcmp $n,$n)
> @echo 0_2 $(intcmp $z,$z)
> @echo 0_3 $(intcmp -$z,$z)
> @echo 0_4 $(intcmp $p,$p)
> @echo 0_5 $(intcmp $n,$z)
> @echo 0_6 $(intcmp $z,$n)
> @echo 1_1 $(intcmp $n,$n,$(shell echo lt))
> @echo 1_2 $(intcmp $n,$z,$(shell echo lt))
> @echo 1_3 $(intcmp $z,$n,$(shell echo lt))
> @echo 2_1 $(intcmp $n,$p,lt,ge)
> @echo 2_2 $(intcmp $z,$z,lt,ge)
> @echo 2_3 $(intcmp $p,$n,lt,ge)
> @echo 3_0 $(intcmp $p,$n,lt,eq,)
> @echo 3_1 $(intcmp $z,$p,lt,eq,gt)
> @echo 3_2 $(intcmp $p,$z,lt,eq,gt)
> @echo 3_3 $(intcmp $p,$p,lt,eq,gt)
> @echo 4_0 $(intcmp $(min),$(max),lt,eq,gt)
> @echo 4_1 $(intcmp $(max),$(min),lt,eq,gt)
> @echo 4_2 $(intcmp $(min),$(min),lt,eq,gt)
> @echo 4_3 $(intcmp $(max),$(max),lt,eq,gt)
> @echo 5_0 $(intcmp -$(huge),$(huge),lt,eq,gt)
> @echo 5_1 $(intcmp $(huge),-$(huge),lt,eq,gt)
> @echo 5_2 $(intcmp -$(huge),-$(huge),lt,eq,gt)
> @echo 5_3 $(intcmp +$(huge),$(huge),lt,eq,gt)
