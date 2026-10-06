// Matched single-threaded integer kernel, signed 64-bit throughout.
class Kernel { public static void main(String[] args) { long x = 7; for (long i=0; i<10000000; i++) x=(x*17+i)%1000000007; System.out.println(x); } }
