"""Read real8 and emulate only the host side of the antenna switch contract."""
import json
import os
import socket
import struct
import threading
import time


class Sinks:
    def __init__(self, ip, folder):
        self.folder=folder
        self.stop=threading.Event()
        self.video={}
        self.fields={}
        self.missing=False
        self.threads=[]
        self.sockets=[]
        for port, handler in [(18008,self.route),(18009,self.real8)]:
            s=socket.socket()
            s.bind((ip,port))
            s.listen()
            s.settimeout(.1)
            self.sockets.append(s)
            t=threading.Thread(target=self.serve,args=(s,handler),daemon=True)
            self.threads.append(t)
            t.start()

    def serve(self,s,handler):
        while not self.stop.is_set():
            try:
                conn,_=s.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with conn:
                conn.settimeout(3)
                try:
                    handler(conn)
                except (OSError, EOFError):
                    continue

    def route(self,conn):
        with conn.makefile("rb") as stream:
            event=json.loads(stream.readline(1024))
        if event.get("event") == "prepare_path":
            from planner_row99 import bench
            assert event["ack_version"] == 3 and event["source"] == "ARM"
            start=time.monotonic_ns()
            boot,gen,version,antenna,path=(event[k] for k in ("boot_id","generation","version","route","required_path"))
            conn.sendall(f"ACK3 {boot} {gen-1} {version} {antenna} {path}\n".encode())
            time.sleep(.08)
            api=bench.accepted
            status=api.request("http://192.168.2.1:18080","GET","/api/system/control",timeout=2)
            gate=api.ssh("cat /sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown")
            assert status["phase"]=="waiting_path_ack" and not status["actual"]["running"] and gate.strip()=="1"
            withheld=os.environ.get("PLANNER_BENCH_PATH_TIMEOUT")=="1"
            with (self.folder/"path-events.ndjson").open("a") as out:
                print(json.dumps(dict(event=event,host_received_ns=start,host_before_ack_ns=time.monotonic_ns(),
                    phase=status["phase"],prior_running=status["actual"]["running"],gate=int(gate),
                    correct_ack=not withheld,physical_switch=False)),file=out)
            if withheld:
                self.stop.wait(11)
            else:
                conn.sendall(f"ACK3 {boot} {gen} {version} {antenna} {path}\n".encode())
            return
        assert event["ack_version"]==2
        fields=[event[k] for k in ("generation","version","epoch","position","marker_id","route")]
        def ack(f):
            return ("ACK2 "+" ".join(map(str,f))+"\n").encode()
        old=fields.copy()
        old[0]-=1
        wrong=fields.copy()
        wrong[4]+=1
        conn.sendall(ack(old)+ack(wrong))
        received=time.monotonic_ns()
        time.sleep(.025)
        if not self.missing:
            conn.sendall(ack(fields)*2)
        with (self.folder/"antenna.ndjson").open("a") as out:
            print(json.dumps(dict(event=event,host_ns=received,correct_ack=not self.missing,
                                  physical_switch=False)),file=out)
        if self.missing:
            self.stop.wait(2.3)

    def real8(self,conn):
        from sdr.source.field1_stream import Field1Stream
        field = Field1Stream()
        with conn.makefile("rb") as stream:
            while not self.stop.is_set():
                header=stream.read(12)
                if not header:return
                assert header[:4] in (b"R8V1", b"R8P2")
                m,n=struct.unpack_from("<II",header,4)
                assert m<16384 and n<=4*1024*1024
                meta=json.loads(stream.read(m))
                block=stream.read(n)
                if len(block)!=n:return
                if header[:4]==b"R8P2":
                    field.accept(meta, block)
                    if meta["kind"] != "data":
                        with (self.folder/"field.ndjson").open("a") as out:
                            print(json.dumps(meta),file=out)
                    if meta["kind"] == "end":
                        self.fields[meta["watch_generation"]] = meta
                    continue
                gen=meta["generation"]
                if gen not in self.video:
                    (self.folder/f"real8-{gen}.bin").write_bytes(block)
                    (self.folder/f"real8-{gen}.json").write_text(json.dumps(meta))
                self.video[gen]=self.video.get(gen,0)+n

    def close(self):
        self.stop.set()
        for s in self.sockets:s.close()
        for t in self.threads:t.join(3)
