// Programmatic SPIR-V 1.0 binary compute shader assembler for Kovrek's Vulkan backend.
// Produces self-contained, spec-compliant compute kernels using DescriptorSet 0
// storage buffers (bindings 0=A, 1=B, 2=Out via SPIR-V 1.0 BufferBlock) and
// PushConstant parameters so no external shader compiler binary is required.

#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;

const SPIRV_MAGIC: u32 = 0x0723_0203;
const SPIRV_VERSION_1_0: u32 = 0x0001_0000;

const OP_EXT_INST_IMPORT: u16 = 11;
const OP_EXT_INST: u16 = 12;
const OP_MEMORY_MODEL: u16 = 14;
const OP_ENTRY_POINT: u16 = 15;
const OP_EXECUTION_MODE: u16 = 16;
const OP_CAPABILITY: u16 = 17;
const OP_TYPE_VOID: u16 = 19;
const OP_TYPE_BOOL: u16 = 20;
const OP_TYPE_INT: u16 = 21;
const OP_TYPE_FLOAT: u16 = 22;
const OP_TYPE_VECTOR: u16 = 23;
const OP_TYPE_ARRAY: u16 = 28;
const OP_TYPE_RUNTIME_ARRAY: u16 = 29;
const OP_TYPE_STRUCT: u16 = 30;
const OP_TYPE_POINTER: u16 = 32;
const OP_TYPE_FUNCTION: u16 = 33;
const OP_CONSTANT: u16 = 43;
const OP_FUNCTION: u16 = 54;
const OP_FUNCTION_END: u16 = 56;
const OP_VARIABLE: u16 = 59;
const OP_LOAD: u16 = 61;
const OP_STORE: u16 = 62;
const OP_ACCESS_CHAIN: u16 = 65;
const OP_DECORATE: u16 = 71;
const OP_MEMBER_DECORATE: u16 = 72;
const OP_CONVERT_U_TO_F: u16 = 112;
const OP_BITCAST: u16 = 124;
const OP_FNEGATE: u16 = 127;
const OP_IADD: u16 = 128;
const OP_FADD: u16 = 129;
const OP_FSUB: u16 = 131;
const OP_IMUL: u16 = 132;
const OP_FMUL: u16 = 133;
const OP_UDIV: u16 = 134;
const OP_FDIV: u16 = 136;
const OP_UMOD: u16 = 138;
const OP_LOGICAL_AND: u16 = 167;
const OP_SELECT: u16 = 169;
const OP_IEQUAL: u16 = 170;
const OP_UGREATER_THAN_EQUAL: u16 = 175;
const OP_ULESS_THAN: u16 = 176;
const OP_FORD_GREATER_THAN: u16 = 186;
const OP_CONTROL_BARRIER: u16 = 224;
const OP_PHI: u16 = 245;
const OP_LOOP_MERGE: u16 = 246;
const OP_SELECTION_MERGE: u16 = 247;
const OP_LABEL: u16 = 248;
const OP_BRANCH: u16 = 249;
const OP_BRANCH_CONDITIONAL: u16 = 250;
const OP_RETURN: u16 = 253;

const CAP_SHADER: u32 = 1;
const ADDR_LOGICAL: u32 = 0;
const MEM_GLSL450: u32 = 1;
const EXEC_GLCOMPUTE: u32 = 5;
const MODE_LOCAL_SIZE: u32 = 17;

const SC_INPUT: u32 = 1;
const SC_UNIFORM: u32 = 2;
const SC_WORKGROUP: u32 = 4;
const SC_FUNCTION: u32 = 7;
const SC_PUSH_CONSTANT: u32 = 9;

const DEC_BLOCK: u32 = 2;
const DEC_BUFFER_BLOCK: u32 = 3;
const DEC_ARRAY_STRIDE: u32 = 6;
const DEC_BUILTIN: u32 = 11;
const DEC_BINDING: u32 = 33;
const DEC_DESCRIPTOR_SET: u32 = 34;
const DEC_OFFSET: u32 = 35;
const BUILTIN_LOCAL_INVOCATION_ID: u32 = 27;
const BUILTIN_GLOBAL_INVOCATION_ID: u32 = 28;
const SCOPE_WORKGROUP: u32 = 2;
// AcquireRelease (0x8) | WorkgroupMemory (0x100)
const SEM_WORKGROUP_BARRIER: u32 = 0x108;

const GLSL_POW: u32 = 26;
const GLSL_EXP: u32 = 27;
const GLSL_SQRT: u32 = 31;
const GLSL_FMAX: u32 = 40;

fn push_inst(out: &mut Vec<u32>, opcode: u16, operands: &[u32]) {
    let words = 1 + operands.len();
    out.push(((words as u32) << 16) | (opcode as u32));
    out.extend_from_slice(operands);
}

fn encode_str_words(s: &str) -> Vec<u32> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

struct SpvModule {
    next_id: u32,
    caps: Vec<u32>,
    ext_insts: Vec<u32>,
    mem_model: Vec<u32>,
    entry_points: Vec<u32>,
    exec_modes: Vec<u32>,
    decs: Vec<u32>,
    types_consts_globals: Vec<u32>,
    funcs: Vec<u32>,

    t_void: u32,
    t_bool: u32,
    t_u32: u32,
    t_f32: u32,
    t_ptr_in_u32: u32,
    t_ptr_buf_f32: u32,
    t_ptr_wg_f32: u32,
    t_ptr_pc_u32: u32,

    glsl_id: u32,
    var_gid: u32,
    var_lid: u32,
    var_a: u32,
    var_b: u32,
    var_out: u32,
    var_pc: u32,

    u32_consts: HashMap<u32, u32>,
    f32_consts: HashMap<u32, u32>,
}

impl SpvModule {
    fn new(local_x: u32, local_y: u32, local_z: u32, pc_u32_count: usize) -> Self {
        Self::new_with_opts(local_x, local_y, local_z, pc_u32_count, false)
    }

    fn new_with_opts(
        local_x: u32,
        local_y: u32,
        local_z: u32,
        pc_u32_count: usize,
        use_local_id: bool,
    ) -> Self {
        let mut m = Self {
            next_id: 1,
            caps: Vec::new(),
            ext_insts: Vec::new(),
            mem_model: Vec::new(),
            entry_points: Vec::new(),
            exec_modes: Vec::new(),
            decs: Vec::new(),
            types_consts_globals: Vec::new(),
            funcs: Vec::new(),
            t_void: 0,
            t_bool: 0,
            t_u32: 0,
            t_f32: 0,
            t_ptr_in_u32: 0,
            t_ptr_buf_f32: 0,
            t_ptr_wg_f32: 0,
            t_ptr_pc_u32: 0,
            glsl_id: 0,
            var_gid: 0,
            var_lid: 0,
            var_a: 0,
            var_b: 0,
            var_out: 0,
            var_pc: 0,
            u32_consts: HashMap::new(),
            f32_consts: HashMap::new(),
        };

        push_inst(&mut m.caps, OP_CAPABILITY, &[CAP_SHADER]);

        let glsl_id = m.id();
        m.glsl_id = glsl_id;
        let mut ext_ops = vec![glsl_id];
        ext_ops.extend_from_slice(&encode_str_words("GLSL.std.450"));
        push_inst(&mut m.ext_insts, OP_EXT_INST_IMPORT, &ext_ops);

        push_inst(
            &mut m.mem_model,
            OP_MEMORY_MODEL,
            &[ADDR_LOGICAL, MEM_GLSL450],
        );

        m.t_void = m.id();
        push_inst(&mut m.types_consts_globals, OP_TYPE_VOID, &[m.t_void]);
        m.t_bool = m.id();
        push_inst(&mut m.types_consts_globals, OP_TYPE_BOOL, &[m.t_bool]);
        m.t_u32 = m.id();
        push_inst(&mut m.types_consts_globals, OP_TYPE_INT, &[m.t_u32, 32, 0]);
        m.t_f32 = m.id();
        push_inst(&mut m.types_consts_globals, OP_TYPE_FLOAT, &[m.t_f32, 32]);

        let t_v3u32 = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_TYPE_VECTOR,
            &[t_v3u32, m.t_u32, 3],
        );
        let t_ptr_in_v3 = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_TYPE_POINTER,
            &[t_ptr_in_v3, SC_INPUT, t_v3u32],
        );
        m.t_ptr_in_u32 = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_TYPE_POINTER,
            &[m.t_ptr_in_u32, SC_INPUT, m.t_u32],
        );
        m.var_gid = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_VARIABLE,
            &[t_ptr_in_v3, m.var_gid, SC_INPUT],
        );
        push_inst(
            &mut m.decs,
            OP_DECORATE,
            &[m.var_gid, DEC_BUILTIN, BUILTIN_GLOBAL_INVOCATION_ID],
        );
        if use_local_id {
            m.var_lid = m.id();
            push_inst(
                &mut m.types_consts_globals,
                OP_VARIABLE,
                &[t_ptr_in_v3, m.var_lid, SC_INPUT],
            );
            push_inst(
                &mut m.decs,
                OP_DECORATE,
                &[m.var_lid, DEC_BUILTIN, BUILTIN_LOCAL_INVOCATION_ID],
            );
        }

        let t_rta_f32 = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_TYPE_RUNTIME_ARRAY,
            &[t_rta_f32, m.t_f32],
        );
        push_inst(&mut m.decs, OP_DECORATE, &[t_rta_f32, DEC_ARRAY_STRIDE, 4]);

        let t_buf_struct = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_TYPE_STRUCT,
            &[t_buf_struct, t_rta_f32],
        );
        push_inst(&mut m.decs, OP_DECORATE, &[t_buf_struct, DEC_BUFFER_BLOCK]);
        push_inst(
            &mut m.decs,
            OP_MEMBER_DECORATE,
            &[t_buf_struct, 0, DEC_OFFSET, 0],
        );

        let t_ptr_buf = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_TYPE_POINTER,
            &[t_ptr_buf, SC_UNIFORM, t_buf_struct],
        );
        m.t_ptr_buf_f32 = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_TYPE_POINTER,
            &[m.t_ptr_buf_f32, SC_UNIFORM, m.t_f32],
        );

        m.var_a = m.declare_storage_var(t_ptr_buf, 0);
        m.var_b = m.declare_storage_var(t_ptr_buf, 1);
        m.var_out = m.declare_storage_var(t_ptr_buf, 2);

        m.t_ptr_pc_u32 = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_TYPE_POINTER,
            &[m.t_ptr_pc_u32, SC_PUSH_CONSTANT, m.t_u32],
        );

        let mut pc_members = vec![0u32; pc_u32_count.max(1)];
        pc_members.fill(m.t_u32);
        let t_pc_struct = m.id();
        let mut st_ops = vec![t_pc_struct];
        st_ops.extend_from_slice(&pc_members);
        push_inst(&mut m.types_consts_globals, OP_TYPE_STRUCT, &st_ops);
        push_inst(&mut m.decs, OP_DECORATE, &[t_pc_struct, DEC_BLOCK]);
        for i in 0..pc_members.len() {
            push_inst(
                &mut m.decs,
                OP_MEMBER_DECORATE,
                &[t_pc_struct, i as u32, DEC_OFFSET, (i * 4) as u32],
            );
        }
        let t_ptr_pc = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_TYPE_POINTER,
            &[t_ptr_pc, SC_PUSH_CONSTANT, t_pc_struct],
        );
        m.var_pc = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_VARIABLE,
            &[t_ptr_pc, m.var_pc, SC_PUSH_CONSTANT],
        );

        let fn_main = m.id();
        let t_fn_void = m.id();
        push_inst(
            &mut m.types_consts_globals,
            OP_TYPE_FUNCTION,
            &[t_fn_void, m.t_void],
        );

        let mut ep_ops = vec![EXEC_GLCOMPUTE, fn_main];
        ep_ops.extend_from_slice(&encode_str_words("main"));
        ep_ops.push(m.var_gid);
        if m.var_lid != 0 {
            ep_ops.push(m.var_lid);
        }
        push_inst(&mut m.entry_points, OP_ENTRY_POINT, &ep_ops);
        push_inst(
            &mut m.exec_modes,
            OP_EXECUTION_MODE,
            &[fn_main, MODE_LOCAL_SIZE, local_x, local_y, local_z],
        );

        push_inst(
            &mut m.funcs,
            OP_FUNCTION,
            &[m.t_void, fn_main, 0, t_fn_void],
        );
        let lbl_entry = m.id();
        push_inst(&mut m.funcs, OP_LABEL, &[lbl_entry]);

        let _ = (SC_FUNCTION,);
        m
    }

    fn declare_storage_var(&mut self, t_ptr_buf: u32, binding: u32) -> u32 {
        let var = self.id();
        push_inst(
            &mut self.types_consts_globals,
            OP_VARIABLE,
            &[t_ptr_buf, var, SC_UNIFORM],
        );
        push_inst(&mut self.decs, OP_DECORATE, &[var, DEC_DESCRIPTOR_SET, 0]);
        push_inst(&mut self.decs, OP_DECORATE, &[var, DEC_BINDING, binding]);
        var
    }

    fn id(&mut self) -> u32 {
        let out = self.next_id;
        self.next_id += 1;
        out
    }

    fn c_u32(&mut self, v: u32) -> u32 {
        if let Some(&existing) = self.u32_consts.get(&v) {
            return existing;
        }
        let cid = self.id();
        push_inst(
            &mut self.types_consts_globals,
            OP_CONSTANT,
            &[self.t_u32, cid, v],
        );
        self.u32_consts.insert(v, cid);
        cid
    }

    fn c_f32(&mut self, v: f32) -> u32 {
        let bits = v.to_bits();
        if let Some(&existing) = self.f32_consts.get(&bits) {
            return existing;
        }
        let cid = self.id();
        push_inst(
            &mut self.types_consts_globals,
            OP_CONSTANT,
            &[self.t_f32, cid, bits],
        );
        self.f32_consts.insert(bits, cid);
        cid
    }

    fn op(&mut self, opcode: u16, operands: &[u32]) {
        push_inst(&mut self.funcs, opcode, operands);
    }

    fn label(&mut self, id: u32) {
        self.op(OP_LABEL, &[id]);
    }

    fn gid_comp(&mut self, axis: u32) -> u32 {
        let c = self.c_u32(axis);
        let ptr = self.id();
        self.op(OP_ACCESS_CHAIN, &[self.t_ptr_in_u32, ptr, self.var_gid, c]);
        let val = self.id();
        self.op(OP_LOAD, &[self.t_u32, val, ptr]);
        val
    }

    fn load_pc_u32(&mut self, slot: u32) -> u32 {
        let c = self.c_u32(slot);
        let ptr = self.id();
        self.op(OP_ACCESS_CHAIN, &[self.t_ptr_pc_u32, ptr, self.var_pc, c]);
        let val = self.id();
        self.op(OP_LOAD, &[self.t_u32, val, ptr]);
        val
    }

    fn load_pc_f32(&mut self, slot: u32) -> u32 {
        let bits = self.load_pc_u32(slot);
        let f = self.id();
        self.op(OP_BITCAST, &[self.t_f32, f, bits]);
        f
    }

    fn load_buf(&mut self, var: u32, idx: u32) -> u32 {
        let c0 = self.c_u32(0);
        let ptr = self.id();
        self.op(OP_ACCESS_CHAIN, &[self.t_ptr_buf_f32, ptr, var, c0, idx]);
        let val = self.id();
        self.op(OP_LOAD, &[self.t_f32, val, ptr]);
        val
    }

    fn store_out(&mut self, idx: u32, val: u32) {
        let c0 = self.c_u32(0);
        let ptr = self.id();
        self.op(
            OP_ACCESS_CHAIN,
            &[self.t_ptr_buf_f32, ptr, self.var_out, c0, idx],
        );
        self.op(OP_STORE, &[ptr, val]);
    }

    fn uadd(&mut self, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_IADD, &[self.t_u32, r, a, b]);
        r
    }
    fn umul(&mut self, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_IMUL, &[self.t_u32, r, a, b]);
        r
    }
    fn udiv(&mut self, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_UDIV, &[self.t_u32, r, a, b]);
        r
    }
    fn umod(&mut self, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_UMOD, &[self.t_u32, r, a, b]);
        r
    }
    fn fadd(&mut self, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_FADD, &[self.t_f32, r, a, b]);
        r
    }
    fn fsub(&mut self, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_FSUB, &[self.t_f32, r, a, b]);
        r
    }
    fn fmul(&mut self, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_FMUL, &[self.t_f32, r, a, b]);
        r
    }
    fn fdiv(&mut self, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_FDIV, &[self.t_f32, r, a, b]);
        r
    }
    fn fneg(&mut self, a: u32) -> u32 {
        let r = self.id();
        self.op(OP_FNEGATE, &[self.t_f32, r, a]);
        r
    }
    fn glsl1(&mut self, inst: u32, a: u32) -> u32 {
        let r = self.id();
        self.op(OP_EXT_INST, &[self.t_f32, r, self.glsl_id, inst, a]);
        r
    }
    fn glsl2(&mut self, inst: u32, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_EXT_INST, &[self.t_f32, r, self.glsl_id, inst, a, b]);
        r
    }
    fn lid_comp(&mut self, comp: u32) -> u32 {
        let c = self.c_u32(comp);
        let ptr = self.id();
        self.op(OP_ACCESS_CHAIN, &[self.t_ptr_in_u32, ptr, self.var_lid, c]);
        let val = self.id();
        self.op(OP_LOAD, &[self.t_u32, val, ptr]);
        val
    }
    fn declare_workgroup_f32_array(&mut self, len: u32) -> u32 {
        if self.t_ptr_wg_f32 == 0 {
            let p = self.id();
            push_inst(
                &mut self.types_consts_globals,
                OP_TYPE_POINTER,
                &[p, SC_WORKGROUP, self.t_f32],
            );
            self.t_ptr_wg_f32 = p;
        }
        let len_id = self.c_u32(len);
        let t_arr = self.id();
        push_inst(
            &mut self.types_consts_globals,
            OP_TYPE_ARRAY,
            &[t_arr, self.t_f32, len_id],
        );
        let t_ptr_arr = self.id();
        push_inst(
            &mut self.types_consts_globals,
            OP_TYPE_POINTER,
            &[t_ptr_arr, SC_WORKGROUP, t_arr],
        );
        let var = self.id();
        push_inst(
            &mut self.types_consts_globals,
            OP_VARIABLE,
            &[t_ptr_arr, var, SC_WORKGROUP],
        );
        var
    }
    fn load_wg(&mut self, var: u32, idx: u32) -> u32 {
        let ptr = self.id();
        self.op(OP_ACCESS_CHAIN, &[self.t_ptr_wg_f32, ptr, var, idx]);
        let val = self.id();
        self.op(OP_LOAD, &[self.t_f32, val, ptr]);
        val
    }
    fn store_wg(&mut self, var: u32, idx: u32, val: u32) {
        let ptr = self.id();
        self.op(OP_ACCESS_CHAIN, &[self.t_ptr_wg_f32, ptr, var, idx]);
        self.op(OP_STORE, &[ptr, val]);
    }
    fn workgroup_barrier(&mut self) {
        let exec = self.c_u32(SCOPE_WORKGROUP);
        let mem = self.c_u32(SCOPE_WORKGROUP);
        let sem = self.c_u32(SEM_WORKGROUP_BARRIER);
        self.op(OP_CONTROL_BARRIER, &[exec, mem, sem]);
    }
    fn select_f32(&mut self, cond: u32, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_SELECT, &[self.t_f32, r, cond, a, b]);
        r
    }
    fn select_u32(&mut self, cond: u32, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_SELECT, &[self.t_u32, r, cond, a, b]);
        r
    }
    fn logical_and(&mut self, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_LOGICAL_AND, &[self.t_bool, r, a, b]);
        r
    }
    fn uless(&mut self, a: u32, b: u32) -> u32 {
        let r = self.id();
        self.op(OP_ULESS_THAN, &[self.t_bool, r, a, b]);
        r
    }
    fn u2f(&mut self, a: u32) -> u32 {
        let r = self.id();
        self.op(OP_CONVERT_U_TO_F, &[self.t_f32, r, a]);
        r
    }
    fn for_range_acc3<F: FnOnce(&mut Self, u32, [u32; 3]) -> [u32; 3]>(
        &mut self,
        start: u32,
        end: u32,
        init: [u32; 3],
        step_body: F,
    ) -> [u32; 3] {
        let c1 = self.c_u32(1);
        let pre = self.id();
        let hdr = self.id();
        let body = self.id();
        let cont = self.id();
        let merge = self.id();

        self.op(OP_BRANCH, &[pre]);
        self.label(pre);
        self.op(OP_BRANCH, &[hdr]);

        self.label(hdr);
        let idx = self.id();
        let acc = [self.id(), self.id(), self.id()];
        let idx_next = self.id();
        let acc_next = [self.id(), self.id(), self.id()];
        self.op(OP_PHI, &[self.t_u32, idx, start, pre, idx_next, cont]);
        for i in 0..3 {
            self.op(
                OP_PHI,
                &[self.t_f32, acc[i], init[i], pre, acc_next[i], cont],
            );
        }
        let cond = self.uless(idx, end);
        self.op(OP_LOOP_MERGE, &[merge, cont, 0]);
        self.op(OP_BRANCH_CONDITIONAL, &[cond, body, merge]);

        self.label(body);
        let computed = step_body(self, idx, acc);
        let f0 = self.c_f32(0.0);
        for i in 0..3 {
            self.op(OP_FADD, &[self.t_f32, acc_next[i], computed[i], f0]);
        }
        self.op(OP_BRANCH, &[cont]);

        self.label(cont);
        self.op(OP_IADD, &[self.t_u32, idx_next, idx, c1]);
        self.op(OP_BRANCH, &[hdr]);

        self.label(merge);
        acc
    }
    fn guard_oob(&mut self, cond_oob: u32) {
        let lbl_ret = self.id();
        let lbl_cont = self.id();
        self.op(OP_SELECTION_MERGE, &[lbl_cont, 0]);
        self.op(OP_BRANCH_CONDITIONAL, &[cond_oob, lbl_ret, lbl_cont]);
        self.label(lbl_ret);
        self.op(OP_RETURN, &[]);
        self.label(lbl_cont);
    }

    /// Emits `if (cond) { body(self); return; }` with its own unique merge label.
    fn if_then_return<F: FnOnce(&mut Self)>(&mut self, cond: u32, body: F) {
        let lbl_then = self.id();
        let lbl_next = self.id();
        self.op(OP_SELECTION_MERGE, &[lbl_next, 0]);
        self.op(OP_BRANCH_CONDITIONAL, &[cond, lbl_then, lbl_next]);
        self.label(lbl_then);
        body(self);
        self.op(OP_RETURN, &[]);
        self.label(lbl_next);
    }

    /// Emits a structured `for i in start..end` loop with optional f32 accumulator.
    fn for_range_acc<F: FnOnce(&mut Self, u32, u32) -> u32>(
        &mut self,
        start: u32,
        end: u32,
        init_acc: u32,
        step_body: F,
    ) -> u32 {
        let c1 = self.c_u32(1);
        let pre = self.id();
        let hdr = self.id();
        let body = self.id();
        let cont = self.id();
        let merge = self.id();

        self.op(OP_BRANCH, &[pre]);
        self.label(pre);
        self.op(OP_BRANCH, &[hdr]);

        self.label(hdr);
        let idx = self.id();
        let acc = self.id();
        let idx_next = self.id();
        let acc_next = self.id();
        self.op(OP_PHI, &[self.t_u32, idx, start, pre, idx_next, cont]);
        self.op(OP_PHI, &[self.t_f32, acc, init_acc, pre, acc_next, cont]);
        let cond = self.id();
        self.op(OP_ULESS_THAN, &[self.t_bool, cond, idx, end]);
        self.op(OP_LOOP_MERGE, &[merge, cont, 0]);
        self.op(OP_BRANCH_CONDITIONAL, &[cond, body, merge]);

        self.label(body);
        let computed = step_body(self, idx, acc);
        // Copy result cleanly via FAdd 0? Wait: no OpCopyObject needed if caller writes into acc_next!
        let f0 = self.c_f32(0.0);
        self.op(OP_FADD, &[self.t_f32, acc_next, computed, f0]);
        self.op(OP_BRANCH, &[cont]);

        self.label(cont);
        self.op(OP_IADD, &[self.t_u32, idx_next, idx, c1]);
        self.op(OP_BRANCH, &[hdr]);

        self.label(merge);
        acc
    }

    /// Emits a structured `for i in 0..end` loop with no accumulator.
    fn for_range_void<F: FnOnce(&mut Self, u32)>(&mut self, start: u32, end: u32, body_fn: F) {
        let c1 = self.c_u32(1);
        let pre = self.id();
        let hdr = self.id();
        let body = self.id();
        let cont = self.id();
        let merge = self.id();

        self.op(OP_BRANCH, &[pre]);
        self.label(pre);
        self.op(OP_BRANCH, &[hdr]);

        self.label(hdr);
        let idx = self.id();
        let idx_next = self.id();
        self.op(OP_PHI, &[self.t_u32, idx, start, pre, idx_next, cont]);
        let cond = self.id();
        self.op(OP_ULESS_THAN, &[self.t_bool, cond, idx, end]);
        self.op(OP_LOOP_MERGE, &[merge, cont, 0]);
        self.op(OP_BRANCH_CONDITIONAL, &[cond, body, merge]);

        self.label(body);
        body_fn(self, idx);
        self.op(OP_BRANCH, &[cont]);

        self.label(cont);
        self.op(OP_IADD, &[self.t_u32, idx_next, idx, c1]);
        self.op(OP_BRANCH, &[hdr]);

        self.label(merge);
    }

    fn finish(mut self) -> Vec<u32> {
        self.op(OP_RETURN, &[]);
        self.op(OP_FUNCTION_END, &[]);
        let mut words = Vec::with_capacity(512);
        words.push(SPIRV_MAGIC);
        words.push(SPIRV_VERSION_1_0);
        words.push(0);
        words.push(self.next_id);
        words.push(0);
        words.extend_from_slice(&self.caps);
        words.extend_from_slice(&self.ext_insts);
        words.extend_from_slice(&self.mem_model);
        words.extend_from_slice(&self.entry_points);
        words.extend_from_slice(&self.exec_modes);
        words.extend_from_slice(&self.decs);
        words.extend_from_slice(&self.types_consts_globals);
        words.extend_from_slice(&self.funcs);
        words
    }
}

/// Generates SPIR-V for `ewise`:
/// PushConstants: [len: u32, op_code: u32, scalar_f32_bits: u32, pad: u32]
/// op_code: 0=add, 1=sub, 2=mul, 3=relu, 4=sigmoid, 5=scale, 6=gelu
pub(crate) fn spirv_ewise() -> Vec<u32> {
    let mut m = SpvModule::new(256, 1, 1, 4);
    let len = m.load_pc_u32(0);
    let op_code = m.load_pc_u32(1);
    let scalar = m.load_pc_f32(2);
    let i = m.gid_comp(0);

    let oob = m.id();
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob, i, len]);
    m.guard_oob(oob);

    let a = m.load_buf(m.var_a, i);
    let c0 = m.c_u32(0);
    let c1 = m.c_u32(1);
    let c2 = m.c_u32(2);
    let c3 = m.c_u32(3);
    let c4 = m.c_u32(4);
    let c5 = m.c_u32(5);
    let c6 = m.c_u32(6);

    let f0 = m.c_f32(0.0);
    let f1 = m.c_f32(1.0);
    let f2 = m.c_f32(2.0);
    let f_half = m.c_f32(0.5);
    let f_k0 = m.c_f32(0.797_884_6); // sqrt(2 / pi)
    let f_k1 = m.c_f32(0.044_715);

    let is0 = m.id();
    let is1 = m.id();
    let is2 = m.id();
    let is3 = m.id();
    let is4 = m.id();
    let is5 = m.id();
    let is6 = m.id();
    m.op(OP_IEQUAL, &[m.t_bool, is0, op_code, c0]);
    m.op(OP_IEQUAL, &[m.t_bool, is1, op_code, c1]);
    m.op(OP_IEQUAL, &[m.t_bool, is2, op_code, c2]);
    m.op(OP_IEQUAL, &[m.t_bool, is3, op_code, c3]);
    m.op(OP_IEQUAL, &[m.t_bool, is4, op_code, c4]);
    m.op(OP_IEQUAL, &[m.t_bool, is5, op_code, c5]);
    m.op(OP_IEQUAL, &[m.t_bool, is6, op_code, c6]);

    // Sequential structured if-return blocks (unique merge block per branch)
    m.if_then_return(is0, |m| {
        let b = m.load_buf(m.var_b, i);
        let r = m.fadd(a, b);
        m.store_out(i, r);
    });
    m.if_then_return(is1, |m| {
        let b = m.load_buf(m.var_b, i);
        let r = m.fsub(a, b);
        m.store_out(i, r);
    });
    m.if_then_return(is2, |m| {
        let b = m.load_buf(m.var_b, i);
        let r = m.fmul(a, b);
        m.store_out(i, r);
    });
    m.if_then_return(is3, |m| {
        let r = m.glsl2(GLSL_FMAX, a, f0);
        m.store_out(i, r);
    });
    m.if_then_return(is4, |m| {
        let neg_a = m.fneg(a);
        let exp_neg = m.glsl1(GLSL_EXP, neg_a);
        let denom = m.fadd(f1, exp_neg);
        let r = m.fdiv(f1, denom);
        m.store_out(i, r);
    });
    m.if_then_return(is5, |m| {
        let r = m.fmul(a, scalar);
        m.store_out(i, r);
    });
    m.if_then_return(is6, |m| {
        // gelu(x) = 0.5 * x * (1 + tanh(y)), tanh(y) = 1 - 2/(exp(2y) + 1) when 2y <= 18 else 1
        let x2 = m.fmul(a, a);
        let x3 = m.fmul(x2, a);
        let cx3 = m.fmul(f_k1, x3);
        let inner = m.fadd(a, cx3);
        let y = m.fmul(f_k0, inner);
        let y2 = m.fmul(f2, y);
        let f18 = m.c_f32(18.0);
        let large_pos = m.id();
        m.op(OP_FORD_GREATER_THAN, &[m.t_bool, large_pos, y2, f18]);
        m.if_then_return(large_pos, |m| {
            m.store_out(i, a);
        });
        let f_neg18 = m.c_f32(-18.0);
        let safe_y2 = m.glsl2(GLSL_FMAX, y2, f_neg18);
        let e2y = m.glsl1(GLSL_EXP, safe_y2);
        let ep1 = m.fadd(e2y, f1);
        let two_over = m.fdiv(f2, ep1);
        let one_plus_tanh = m.fsub(f2, two_over);
        let half_x = m.fmul(f_half, a);
        let r = m.fmul(half_x, one_plus_tanh);
        m.store_out(i, r);
    });

    m.store_out(i, a);
    m.finish()
}

/// Generates SPIR-V for matrix multiply `C[M, N] = A[M, K] * B[K, N]`.
/// PushConstants: [M, K, N, pad], LocalSize(16, 16, 1).
pub(crate) fn spirv_matmul() -> Vec<u32> {
    let mut m = SpvModule::new(16, 16, 1, 4);
    let dim_m = m.load_pc_u32(0);
    let dim_k = m.load_pc_u32(1);
    let dim_n = m.load_pc_u32(2);

    let col = m.gid_comp(0);
    let row = m.gid_comp(1);

    let oob_r = m.id();
    let oob_c = m.id();
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob_r, row, dim_m]);
    m.guard_oob(oob_r);
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob_c, col, dim_n]);
    m.guard_oob(oob_c);

    let row_k = m.umul(row, dim_k);
    let c0 = m.c_u32(0);
    let f0 = m.c_f32(0.0);

    let acc = m.for_range_acc(c0, dim_k, f0, |m, p_idx, acc| {
        let a_idx = m.uadd(row_k, p_idx);
        let p_n = m.umul(p_idx, dim_n);
        let b_idx = m.uadd(p_n, col);
        let av = m.load_buf(m.var_a, a_idx);
        let bv = m.load_buf(m.var_b, b_idx);
        let prod = m.fmul(av, bv);
        m.fadd(acc, prod)
    });

    let row_n = m.umul(row, dim_n);
    let out_idx = m.uadd(row_n, col);
    m.store_out(out_idx, acc);
    m.finish()
}

/// Generates SPIR-V for row-wise softmax over `Rows x Cols`.
/// PushConstants: [Rows, Cols, 0, 0], LocalSize(64, 1, 1).
pub(crate) fn spirv_softmax() -> Vec<u32> {
    let mut m = SpvModule::new(64, 1, 1, 4);
    let rows = m.load_pc_u32(0);
    let cols = m.load_pc_u32(1);
    let r = m.gid_comp(0);

    let c0 = m.c_u32(0);
    let c1 = m.c_u32(1);
    let f0 = m.c_f32(0.0);

    let oob = m.id();
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob, r, rows]);
    m.guard_oob(oob);
    let cols_zero = m.id();
    m.op(OP_IEQUAL, &[m.t_bool, cols_zero, cols, c0]);
    m.guard_oob(cols_zero);

    let base = m.umul(r, cols);
    let init_mx = m.load_buf(m.var_a, base);

    let mx = m.for_range_acc(c1, cols, init_mx, |m, c_idx, mx| {
        let idx = m.uadd(base, c_idx);
        let v = m.load_buf(m.var_a, idx);
        m.glsl2(GLSL_FMAX, mx, v)
    });

    let sum = m.for_range_acc(c0, cols, f0, |m, c_idx, sum| {
        let idx = m.uadd(base, c_idx);
        let v = m.load_buf(m.var_a, idx);
        let shifted = m.fsub(v, mx);
        let e = m.glsl1(GLSL_EXP, shifted);
        m.store_out(idx, e);
        m.fadd(sum, e)
    });

    m.for_range_void(c0, cols, |m, c_idx| {
        let idx = m.uadd(base, c_idx);
        let e_val = m.load_buf(m.var_out, idx);
        let norm = m.fdiv(e_val, sum);
        m.store_out(idx, norm);
    });

    m.finish()
}

/// Generates SPIR-V for row-wise LayerNorm over `Rows x Cols`:
/// `out[r, c] = (in[r, c] - mean_r) / sqrt(var_r + eps)`
/// PushConstants: [Rows, Cols, eps_f32_bits, 0], LocalSize(64, 1, 1).
pub(crate) fn spirv_layernorm() -> Vec<u32> {
    let mut m = SpvModule::new(64, 1, 1, 4);
    let rows = m.load_pc_u32(0);
    let cols = m.load_pc_u32(1);
    let eps = m.load_pc_f32(2);
    let r = m.gid_comp(0);

    let c0 = m.c_u32(0);
    let f0 = m.c_f32(0.0);

    let oob = m.id();
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob, r, rows]);
    m.guard_oob(oob);
    let cols_zero = m.id();
    m.op(OP_IEQUAL, &[m.t_bool, cols_zero, cols, c0]);
    m.guard_oob(cols_zero);

    let cols_f = m.id();
    m.op(112, &[m.t_f32, cols_f, cols]); // OpConvertUToF
    let base = m.umul(r, cols);

    let sum = m.for_range_acc(c0, cols, f0, |m, c_idx, acc| {
        let idx = m.uadd(base, c_idx);
        let v = m.load_buf(m.var_a, idx);
        m.fadd(acc, v)
    });
    let mean = m.fdiv(sum, cols_f);

    let sqsum = m.for_range_acc(c0, cols, f0, |m, c_idx, acc| {
        let idx = m.uadd(base, c_idx);
        let v = m.load_buf(m.var_a, idx);
        let diff = m.fsub(v, mean);
        let sq = m.fmul(diff, diff);
        m.fadd(acc, sq)
    });
    let var = m.fdiv(sqsum, cols_f);
    let var_eps = m.fadd(var, eps);
    let std = m.glsl1(GLSL_SQRT, var_eps);

    m.for_range_void(c0, cols, |m, c_idx| {
        let idx = m.uadd(base, c_idx);
        let v = m.load_buf(m.var_a, idx);
        let centered = m.fsub(v, mean);
        let norm = m.fdiv(centered, std);
        m.store_out(idx, norm);
    });

    m.finish()
}

/// Generates SPIR-V for valid-padding 2D convolution (`NCHW`).
/// PushConstants: [N, C, H, W, O, KH, KW, Stride, OH, OW, 0, 0], LocalSize(64, 1, 1).
pub(crate) fn spirv_conv2d() -> Vec<u32> {
    let mut m = SpvModule::new(64, 1, 1, 12);
    let dim_n = m.load_pc_u32(0);
    let dim_c = m.load_pc_u32(1);
    let dim_h = m.load_pc_u32(2);
    let dim_w = m.load_pc_u32(3);
    let dim_o = m.load_pc_u32(4);
    let dim_kh = m.load_pc_u32(5);
    let dim_kw = m.load_pc_u32(6);
    let stride = m.load_pc_u32(7);
    let dim_oh = m.load_pc_u32(8);
    let dim_ow = m.load_pc_u32(9);

    let t1 = m.umul(dim_n, dim_o);
    let t2 = m.umul(t1, dim_oh);
    let total = m.umul(t2, dim_ow);
    let idx = m.gid_comp(0);

    let oob = m.id();
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob, idx, total]);
    m.guard_oob(oob);

    let ox = m.umod(idx, dim_ow);
    let rem1 = m.udiv(idx, dim_ow);
    let oy = m.umod(rem1, dim_oh);
    let rem2 = m.udiv(rem1, dim_oh);
    let oc = m.umod(rem2, dim_o);
    let b = m.udiv(rem2, dim_o);

    let c0 = m.c_u32(0);
    let f0 = m.c_f32(0.0);

    let b_c = m.umul(b, dim_c);
    let oc_c = m.umul(oc, dim_c);
    let oy_s = m.umul(oy, stride);
    let ox_s = m.umul(ox, stride);

    let acc = m.for_range_acc(c0, dim_c, f0, |m, ic, acc_c| {
        let bc_ic = m.uadd(b_c, ic);
        let in_plane = m.umul(bc_ic, dim_h);
        let oc_ic = m.uadd(oc_c, ic);
        let wt_plane = m.umul(oc_ic, dim_kh);

        m.for_range_acc(c0, dim_kh, acc_c, |m, ky, acc_y| {
            let iy = m.uadd(oy_s, ky);
            let in_row = m.uadd(in_plane, iy);
            let in_row_w = m.umul(in_row, dim_w);
            let wt_row = m.uadd(wt_plane, ky);
            let wt_row_w = m.umul(wt_row, dim_kw);

            m.for_range_acc(c0, dim_kw, acc_y, |m, kx, acc_x| {
                let ix = m.uadd(ox_s, kx);
                let in_i = m.uadd(in_row_w, ix);
                let w_i = m.uadd(wt_row_w, kx);
                let iv = m.load_buf(m.var_a, in_i);
                let wv = m.load_buf(m.var_b, w_i);
                let prod = m.fmul(iv, wv);
                m.fadd(acc_x, prod)
            })
        })
    });

    m.store_out(idx, acc);
    m.finish()
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum FusedEwiseOp {
    Add,
    Sub,
    Mul,
    Div,
    AddScalar(f32),
    Scale(f32),
    Abs,
    Neg,
    Sqrt,
    Relu,
    Sigmoid,
    Silu,
    Gelu,
    Tanh,
    Clamp(f32, f32),
    PowScalar(f32),
    Recip,
}

fn spirv_sigmoid(m: &mut SpvModule, x: u32) -> u32 {
    let f1 = m.c_f32(1.0);
    let neg = m.fneg(x);
    let e = m.glsl1(GLSL_EXP, neg);
    let denom = m.fadd(f1, e);
    m.fdiv(f1, denom)
}

fn spirv_silu(m: &mut SpvModule, x: u32) -> u32 {
    let sig = spirv_sigmoid(m, x);
    m.fmul(x, sig)
}

fn spirv_clamp(m: &mut SpvModule, x: u32, lo: f32, hi: f32) -> u32 {
    let f_lo = m.c_f32(lo);
    let f_hi = m.c_f32(hi);
    let ge_lo = m.glsl2(GLSL_FMAX, x, f_lo);
    let gt_hi = m.id();
    m.op(OP_FORD_GREATER_THAN, &[m.t_bool, gt_hi, ge_lo, f_hi]);
    let clamped = m.id();
    m.op(169u16, &[m.t_f32, clamped, gt_hi, f_hi, ge_lo]);
    clamped
}

fn spirv_tanh(m: &mut SpvModule, x: u32) -> u32 {
    let f1 = m.c_f32(1.0);
    let f2 = m.c_f32(2.0);
    let two_x = m.fmul(f2, x);
    let clamped = spirv_clamp(m, two_x, -18.0, 18.0);
    let ex = m.glsl1(GLSL_EXP, clamped);
    let ep1 = m.fadd(ex, f1);
    let two_over = m.fdiv(f2, ep1);
    m.fsub(f1, two_over)
}

fn spirv_gelu(m: &mut SpvModule, x: u32) -> u32 {
    let f2 = m.c_f32(2.0);
    let f_half = m.c_f32(0.5);
    let f_k0 = m.c_f32(0.797_884_6);
    let f_k1 = m.c_f32(0.044_715);
    let f1 = m.c_f32(1.0);
    let x2 = m.fmul(x, x);
    let x3 = m.fmul(x2, x);
    let cx3 = m.fmul(f_k1, x3);
    let inner = m.fadd(x, cx3);
    let y = m.fmul(f_k0, inner);
    let y2 = m.fmul(f2, y);
    let clamped = spirv_clamp(m, y2, -18.0, 18.0);
    let ex = m.glsl1(GLSL_EXP, clamped);
    let ep1 = m.fadd(ex, f1);
    let two_over = m.fdiv(f2, ep1);
    let one_plus_tanh = m.fsub(f2, two_over);
    let half_x = m.fmul(f_half, x);
    m.fmul(half_x, one_plus_tanh)
}

/// Compiles an elementwise DAG sequence into a single straight-line SPIR-V 1.0 compute kernel
/// executing all fused operations in SSA registers with zero intermediate buffer writes.
pub(crate) fn spirv_fused_ewise(ops: &[FusedEwiseOp]) -> Vec<u32> {
    let mut m = SpvModule::new(256, 1, 1, 1);
    let len = m.load_pc_u32(0);
    let i = m.gid_comp(0);

    let oob = m.id();
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob, i, len]);
    m.guard_oob(oob);

    let mut acc = m.load_buf(m.var_a, i);
    let mut loaded_b: Option<u32> = None;
    let mut get_b = |m: &mut SpvModule| -> u32 {
        if let Some(id) = loaded_b {
            id
        } else {
            let id = m.load_buf(m.var_b, i);
            loaded_b = Some(id);
            id
        }
    };

    for &op in ops {
        acc = match op {
            FusedEwiseOp::Add => {
                let b = get_b(&mut m);
                m.fadd(acc, b)
            }
            FusedEwiseOp::Sub => {
                let b = get_b(&mut m);
                m.fsub(acc, b)
            }
            FusedEwiseOp::Mul => {
                let b = get_b(&mut m);
                m.fmul(acc, b)
            }
            FusedEwiseOp::Div => {
                let b = get_b(&mut m);
                m.fdiv(acc, b)
            }
            FusedEwiseOp::AddScalar(s) => {
                let c = m.c_f32(s);
                m.fadd(acc, c)
            }
            FusedEwiseOp::Scale(s) => {
                let c = m.c_f32(s);
                m.fmul(acc, c)
            }
            FusedEwiseOp::Neg => m.fneg(acc),
            FusedEwiseOp::Recip => {
                let f1 = m.c_f32(1.0);
                m.fdiv(f1, acc)
            }
            FusedEwiseOp::Abs => {
                let neg = m.fneg(acc);
                m.glsl2(GLSL_FMAX, acc, neg)
            }
            FusedEwiseOp::Sqrt => {
                let f0 = m.c_f32(0.0);
                let safe = m.glsl2(GLSL_FMAX, acc, f0);
                m.glsl1(GLSL_SQRT, safe)
            }
            FusedEwiseOp::Relu => {
                let f0 = m.c_f32(0.0);
                m.glsl2(GLSL_FMAX, acc, f0)
            }
            FusedEwiseOp::Sigmoid => spirv_sigmoid(&mut m, acc),
            FusedEwiseOp::Silu => spirv_silu(&mut m, acc),
            FusedEwiseOp::Tanh => spirv_tanh(&mut m, acc),
            FusedEwiseOp::Gelu => spirv_gelu(&mut m, acc),
            FusedEwiseOp::Clamp(lo, hi) => spirv_clamp(&mut m, acc, lo.min(hi), lo.max(hi)),
            FusedEwiseOp::PowScalar(p) => {
                if p == 0.0 {
                    m.c_f32(1.0)
                } else if p == 1.0 {
                    acc
                } else if p == 2.0 {
                    m.fmul(acc, acc)
                } else if p == 3.0 {
                    let x2 = m.fmul(acc, acc);
                    m.fmul(x2, acc)
                } else {
                    let f0 = m.c_f32(0.0);
                    let safe = m.glsl2(GLSL_FMAX, acc, f0);
                    let c_p = m.c_f32(p);
                    m.glsl2(GLSL_POW, safe, c_p)
                }
            }
        };
    }

    m.store_out(i, acc);
    m.finish()
}

/// Generates single-pass SPIR-V for fused `Y[M, N] = act(X[M, K] @ W[K, N] + bias[N])`.
/// Buffer B packs `[W (K*N floats), bias (N floats)]` when `has_bias == true`.
/// `act_code`: 0=none, 1=relu, 2=gelu, 3=silu. PushConstants: `[M, K, N, 0]`, LocalSize(16, 16, 1).
pub(crate) fn spirv_fused_linear(act_code: u32, has_bias: bool) -> Vec<u32> {
    let mut m = SpvModule::new(16, 16, 1, 4);
    let dim_m = m.load_pc_u32(0);
    let dim_k = m.load_pc_u32(1);
    let dim_n = m.load_pc_u32(2);

    let col = m.gid_comp(0);
    let row = m.gid_comp(1);

    let oob_r = m.id();
    let oob_c = m.id();
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob_r, row, dim_m]);
    m.guard_oob(oob_r);
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob_c, col, dim_n]);
    m.guard_oob(oob_c);

    let row_k = m.umul(row, dim_k);
    let c0 = m.c_u32(0);
    let f0 = m.c_f32(0.0);

    let mut acc = m.for_range_acc(c0, dim_k, f0, |m, p_idx, acc| {
        let a_idx = m.uadd(row_k, p_idx);
        let p_n = m.umul(p_idx, dim_n);
        let b_idx = m.uadd(p_n, col);
        let av = m.load_buf(m.var_a, a_idx);
        let bv = m.load_buf(m.var_b, b_idx);
        let prod = m.fmul(av, bv);
        m.fadd(acc, prod)
    });

    if has_bias {
        let kn = m.umul(dim_k, dim_n);
        let bias_idx = m.uadd(kn, col);
        let bv = m.load_buf(m.var_b, bias_idx);
        acc = m.fadd(acc, bv);
    }

    acc = match act_code {
        1 => m.glsl2(GLSL_FMAX, acc, f0),
        2 => spirv_gelu(&mut m, acc),
        3 => spirv_silu(&mut m, acc),
        _ => acc,
    };

    let row_n = m.umul(row, dim_n);
    let out_idx = m.uadd(row_n, col);
    m.store_out(out_idx, acc);
    m.finish()
}

impl SpvModule {
    /// Emits `if (in_bounds) { load_buf(buf, idx) } else { fallback }` without touching OOB memory.
    fn cond_load_buf(&mut self, in_bounds: u32, buf: u32, idx: u32, fallback: u32) -> u32 {
        let lbl_then = self.id();
        let lbl_else = self.id();
        let lbl_merge = self.id();
        self.op(OP_SELECTION_MERGE, &[lbl_merge, 0]);
        self.op(OP_BRANCH_CONDITIONAL, &[in_bounds, lbl_then, lbl_else]);
        self.label(lbl_then);
        let loaded = self.load_buf(buf, idx);
        self.op(OP_BRANCH, &[lbl_merge]);
        self.label(lbl_else);
        self.op(OP_BRANCH, &[lbl_merge]);
        self.label(lbl_merge);
        let res = self.id();
        self.op(
            OP_PHI,
            &[self.t_f32, res, loaded, lbl_then, fallback, lbl_else],
        );
        res
    }

    fn if_then<F: FnOnce(&mut Self)>(&mut self, cond: u32, body: F) {
        let lbl_then = self.id();
        let lbl_merge = self.id();
        self.op(OP_SELECTION_MERGE, &[lbl_merge, 0]);
        self.op(OP_BRANCH_CONDITIONAL, &[cond, lbl_then, lbl_merge]);
        self.label(lbl_then);
        body(self);
        self.op(OP_BRANCH, &[lbl_merge]);
        self.label(lbl_merge);
    }
}

/// Generates Workgroup shared-memory tiled GEMM `C[M, N] = A[M, K] @ B[K, N]`.
/// Uses `tile = 16` (`LocalSize(16, 16, 1)`) and `OpControlBarrier` across K-tiles.
/// PushConstants: `[M, K, N, 0]`.
pub(crate) fn spirv_tiled_matmul() -> Vec<u32> {
    const TILE: u32 = 16;
    let mut m = SpvModule::new_with_opts(TILE, TILE, 1, 4, true);
    let dim_m = m.load_pc_u32(0);
    let dim_k = m.load_pc_u32(1);
    let dim_n = m.load_pc_u32(2);

    let tile_a = m.declare_workgroup_f32_array(TILE * TILE);
    let tile_b = m.declare_workgroup_f32_array(TILE * TILE);

    let col = m.gid_comp(0);
    let row = m.gid_comp(1);
    let lx = m.lid_comp(0);
    let ly = m.lid_comp(1);

    let c0 = m.c_u32(0);
    let c15 = m.c_u32(TILE - 1);
    let c16 = m.c_u32(TILE);
    let f0 = m.c_f32(0.0);

    let in_row = m.uless(row, dim_m);
    let in_col = m.uless(col, dim_n);
    let in_out = m.logical_and(in_row, in_col);

    let row_k = m.umul(row, dim_k);
    let ly_16 = m.umul(ly, c16);
    let local_idx = m.uadd(ly_16, lx);

    let k_plus = m.uadd(dim_k, c15);
    let num_tiles = m.udiv(k_plus, c16);

    let acc = m.for_range_acc(c0, num_tiles, f0, |m, t_idx, acc| {
        let t_base = m.umul(t_idx, c16);
        let a_col = m.uadd(t_base, lx);
        let a_in_k = m.uless(a_col, dim_k);
        let load_a_ok = m.logical_and(in_row, a_in_k);
        let a_idx = m.uadd(row_k, a_col);
        let a_val = m.cond_load_buf(load_a_ok, m.var_a, a_idx, f0);
        m.store_wg(tile_a, local_idx, a_val);

        let b_row = m.uadd(t_base, ly);
        let b_in_k = m.uless(b_row, dim_k);
        let load_b_ok = m.logical_and(b_in_k, in_col);
        let b_row_n = m.umul(b_row, dim_n);
        let b_idx = m.uadd(b_row_n, col);
        let b_val = m.cond_load_buf(load_b_ok, m.var_b, b_idx, f0);
        m.store_wg(tile_b, local_idx, b_val);

        m.workgroup_barrier();

        let partial = m.for_range_acc(c0, c16, acc, |m, k_sub, p_acc| {
            let ta_idx = m.uadd(ly_16, k_sub);
            let k_16 = m.umul(k_sub, c16);
            let tb_idx = m.uadd(k_16, lx);
            let av = m.load_wg(tile_a, ta_idx);
            let bv = m.load_wg(tile_b, tb_idx);
            let prod = m.fmul(av, bv);
            m.fadd(p_acc, prod)
        });

        m.workgroup_barrier();
        partial
    });

    m.if_then(in_out, |m| {
        let row_n = m.umul(row, dim_n);
        let out_idx = m.uadd(row_n, col);
        m.store_out(out_idx, acc);
    });
    m.finish()
}

/// Generates FlashAttention-style single-pass online-softmax scaled dot-product attention:
/// `O = softmax(Q @ K^T / sqrt(HeadDim) [+ causal_mask]) @ V`.
/// Buffer A: `Q[SeqQ * HeadDim]`. Buffer B: `[K[SeqK * HeadDim], V[SeqK * HeadDim]]`.
/// PushConstants: `[SeqQ, SeqK, HeadDim, CausalFlag]`, LocalSize(64, 1, 1).
pub(crate) fn spirv_attention(causal: bool) -> Vec<u32> {
    let mut m = SpvModule::new(64, 1, 1, 4);
    let seq_q = m.load_pc_u32(0);
    let seq_k = m.load_pc_u32(1);
    let head_dim = m.load_pc_u32(2);

    let total = m.umul(seq_q, head_dim);
    let gid = m.gid_comp(0);

    let oob = m.id();
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob, gid, total]);
    m.guard_oob(oob);

    let c0 = m.c_u32(0);
    let c1 = m.c_u32(1);
    let hd_zero = m.id();
    m.op(OP_IEQUAL, &[m.t_bool, hd_zero, head_dim, c0]);
    m.guard_oob(hd_zero);

    let q_row = m.udiv(gid, head_dim);
    let d_col = m.umod(gid, head_dim);

    let hd_f = m.u2f(head_dim);
    let sqrt_d = m.glsl1(GLSL_SQRT, hd_f);
    let f1 = m.c_f32(1.0);
    let f0 = m.c_f32(0.0);
    let inv_sqrt_d = m.fdiv(f1, sqrt_d);

    let q_base = m.umul(q_row, head_dim);
    let v_offset = m.umul(seq_k, head_dim);

    let k_limit = if causal {
        let q_plus1 = m.uadd(q_row, c1);
        let q_lt = m.uless(q_plus1, seq_k);
        m.select_u32(q_lt, q_plus1, seq_k)
    } else {
        seq_k
    };

    let f_neg_inf = m.c_f32(-1.0e30);
    let state = m.for_range_acc3(
        c0,
        k_limit,
        [f_neg_inf, f0, f0],
        |m, k_idx, [mx, sum_e, out_a]| {
            let k_base = m.umul(k_idx, head_dim);
            let dot = m.for_range_acc(c0, head_dim, f0, |m, p_idx, d_acc| {
                let qi = m.uadd(q_base, p_idx);
                let ki = m.uadd(k_base, p_idx);
                let qv = m.load_buf(m.var_a, qi);
                let kv = m.load_buf(m.var_b, ki);
                let prod = m.fmul(qv, kv);
                m.fadd(d_acc, prod)
            });
            let score = m.fmul(dot, inv_sqrt_d);
            let new_max = m.glsl2(GLSL_FMAX, mx, score);
            let diff_old = m.fsub(mx, new_max);
            let alpha = m.glsl1(GLSL_EXP, diff_old);
            let diff_new = m.fsub(score, new_max);
            let w = m.glsl1(GLSL_EXP, diff_new);

            let sum_scaled = m.fmul(sum_e, alpha);
            let new_sum = m.fadd(sum_scaled, w);

            let vk_off = m.uadd(v_offset, k_base);
            let vi = m.uadd(vk_off, d_col);
            let vv = m.load_buf(m.var_b, vi);
            let wv = m.fmul(w, vv);
            let out_scaled = m.fmul(out_a, alpha);
            let new_out = m.fadd(out_scaled, wv);
            [new_max, new_sum, new_out]
        },
    );

    let sum_e = state[1];
    let out_a = state[2];
    let pos_sum = m.id();
    m.op(OP_FORD_GREATER_THAN, &[m.t_bool, pos_sum, sum_e, f0]);
    let safe_denom = m.select_f32(pos_sum, sum_e, f1);
    let normed = m.fdiv(out_a, safe_denom);
    let res = m.select_f32(pos_sum, normed, f0);
    m.store_out(gid, res);
    m.finish()
}

/// Generates single-pass fused `Y = act(LayerNorm(X + Residual) * gamma + beta)`.
/// Buffer A: `X[Rows * Cols]`.
/// Buffer B: `[Residual[Rows * Cols] (, Gamma[Cols], Beta[Cols])]`.
/// PushConstants: `[Rows, Cols, eps_f32_bits, 0]`, LocalSize(64, 1, 1).
pub(crate) fn spirv_fused_layernorm_residual(act_code: u32, has_gamma_beta: bool) -> Vec<u32> {
    let mut m = SpvModule::new(64, 1, 1, 4);
    let rows = m.load_pc_u32(0);
    let cols = m.load_pc_u32(1);
    let eps = m.load_pc_f32(2);
    let r = m.gid_comp(0);

    let c0 = m.c_u32(0);
    let f0 = m.c_f32(0.0);

    let oob = m.id();
    m.op(OP_UGREATER_THAN_EQUAL, &[m.t_bool, oob, r, rows]);
    m.guard_oob(oob);
    let cols_zero = m.id();
    m.op(OP_IEQUAL, &[m.t_bool, cols_zero, cols, c0]);
    m.guard_oob(cols_zero);

    let cols_f = m.u2f(cols);
    let base = m.umul(r, cols);
    let rows_cols = m.umul(rows, cols);

    let sum = m.for_range_acc(c0, cols, f0, |m, c_idx, acc| {
        let idx = m.uadd(base, c_idx);
        let xv = m.load_buf(m.var_a, idx);
        let rv = m.load_buf(m.var_b, idx);
        let hv = m.fadd(xv, rv);
        m.fadd(acc, hv)
    });
    let mean = m.fdiv(sum, cols_f);

    let sqsum = m.for_range_acc(c0, cols, f0, |m, c_idx, acc| {
        let idx = m.uadd(base, c_idx);
        let xv = m.load_buf(m.var_a, idx);
        let rv = m.load_buf(m.var_b, idx);
        let hv = m.fadd(xv, rv);
        let diff = m.fsub(hv, mean);
        let sq = m.fmul(diff, diff);
        m.fadd(acc, sq)
    });
    let var = m.fdiv(sqsum, cols_f);
    let var_eps = m.fadd(var, eps);
    let std = m.glsl1(GLSL_SQRT, var_eps);

    let gamma_off = rows_cols;
    let beta_off = m.uadd(rows_cols, cols);

    m.for_range_void(c0, cols, |m, c_idx| {
        let idx = m.uadd(base, c_idx);
        let xv = m.load_buf(m.var_a, idx);
        let rv = m.load_buf(m.var_b, idx);
        let hv = m.fadd(xv, rv);
        let centered = m.fsub(hv, mean);
        let mut norm = m.fdiv(centered, std);
        if has_gamma_beta {
            let gi = m.uadd(gamma_off, c_idx);
            let bi = m.uadd(beta_off, c_idx);
            let gv = m.load_buf(m.var_b, gi);
            let bv = m.load_buf(m.var_b, bi);
            let scaled = m.fmul(norm, gv);
            norm = m.fadd(scaled, bv);
        }
        let activated = match act_code {
            1 => m.glsl2(GLSL_FMAX, norm, f0),
            2 => spirv_gelu(m, norm),
            3 => spirv_silu(m, norm),
            _ => norm,
        };
        m.store_out(idx, activated);
    });

    m.finish()
}
