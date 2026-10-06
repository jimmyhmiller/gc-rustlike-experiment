// Matched single-threaded integer kernel, signed 64-bit throughout.
class Kernel { public static void main(String[] args) { long[] a=new long[1024]; java.util.Arrays.fill(a,7); for(long i=0;i<5000000;i++){int k=(int)(i%1024); a[k]=(a[k]*17+i)%1000000007;} long sum=0;for(long x:a)sum+=x;System.out.println(sum); } }
