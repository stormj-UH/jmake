.SECONDEXPANSION:
all: 2.x
%.x: 5.z 6.z 5.z $$(info @=$$@,<=$$<,^=$$^,+=$$+,|=$$|,?=$$?,*=$$*) ;
%.z: ;
