# Regression test: jmake must abort when a := assignment builds a string
# larger than MAX_EXPANDED_VALUE_BYTES (256 MiB).
#
# Attack: a 1 KiB seed doubled 19 times = 512 MiB would exhaust memory
# without a size guard.  With the guard, expansion aborts at the step
# that would first exceed 256 MiB.
#
# This test verifies the *safe* behavior (error + exit 2).
SEED := aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
S1 := $(SEED)$(SEED)
S2 := $(S1)$(S1)
S3 := $(S2)$(S2)
S4 := $(S3)$(S3)
S5 := $(S4)$(S4)
S6 := $(S5)$(S5)
S7 := $(S6)$(S6)
S8 := $(S7)$(S7)
S9 := $(S8)$(S8)
S10 := $(S9)$(S9)
S11 := $(S10)$(S10)
S12 := $(S11)$(S11)
S13 := $(S12)$(S12)
S14 := $(S13)$(S13)
S15 := $(S14)$(S14)
S16 := $(S15)$(S15)
S17 := $(S16)$(S16)
S18 := $(S17)$(S17)
S19 := $(S18)$(S18)

all:
	@echo should-not-reach
