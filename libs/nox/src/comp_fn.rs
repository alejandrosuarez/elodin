use crate::{
    ArrayTy, Builder, Comp, IntoOp, Noxpr, NoxprFn, Op, Tensor, TensorDim, TensorItem, XlaDim,
};
use paste::paste;
use smallvec::SmallVec;
use std::{any, marker::PhantomData};

pub trait CompFn<T, R>: Send + Sync {
    fn compute(&self, builder: &mut Builder) -> R;

    fn build_expr(&self) -> Result<NoxprFn, crate::Error>
    where
        R: IntoOp,
    {
        let mut builder = Builder::new();
        let res = self.compute(&mut builder);
        let inner = if !builder.mut_params.is_empty() {
            let mut tuple = Vec::with_capacity(builder.mut_params.count() + 1);
            let res_op = res.into_op();
            tuple.push(res_op);
            for o in builder.mut_params.into_iter() {
                tuple.insert(1, o.into_inner().into_op());
            }
            Noxpr::tuple(tuple)
        } else {
            res.into_op()
        };
        Ok(NoxprFn {
            inner,
            args: builder.params.into_inner(),
        })
    }

    fn build(&self) -> Result<Comp<T, R>, crate::Error>
    where
        R: IntoOp,
    {
        let expr = self.build_expr()?;
        let op = expr.build(any::type_name::<Self>())?;
        let comp = op.build()?;
        Ok(Comp {
            comp,
            phantom: PhantomData,
        })
    }
}

pub trait FromBuilder {
    type Item<'a>;

    fn from_builder(builder: &Builder) -> Self::Item<'_>;
    fn is_mut_borrowed() -> bool {
        false
    }
}

impl<'b> FromBuilder for &'b Builder {
    type Item<'a> = &'a Builder;

    fn from_builder(builder: &Builder) -> Self::Item<'_> {
        builder
    }
}

impl<T: TensorItem, D: XlaDim + TensorDim> FromBuilder for Tensor<T, D, Op>
where
    T::Dim: XlaDim,
    <T::Dim as XlaDim>::Array: AsRef<[i64]>,
    D::Array: AsRef<[i64]>,
{
    type Item<'a> = Self;

    fn from_builder(builder: &Builder) -> Self::Item<'_> {
        let mut params = builder.params.borrow_mut();
        let i = params.len() as i64;
        let mut shape = SmallVec::from_slice(D::dims().as_ref());
        shape.extend_from_slice(<T::Dim as XlaDim>::dims().as_ref());
        let inner = Noxpr::parameter(
            i,
            ArrayTy {
                element_type: T::ELEM,
                shape,
            },
            format!("param_{}", i),
        );
        params.push(inner.clone());
        Tensor {
            inner,
            phantom: PhantomData,
        }
    }
}

impl<'b, T: xla::ArrayElement + 'static, D: XlaDim + TensorDim + 'static> FromBuilder
    for &'b mut Tensor<T, D, Op>
where
    D::Array: AsRef<[i64]>,
{
    type Item<'a> = &'a mut Tensor<T, D, Op>;

    fn from_builder(builder: &Builder) -> Self::Item<'_> {
        let mut params = builder.params.borrow_mut();
        let i = params.len() as i64;
        let inner = Noxpr::parameter(
            i,
            ArrayTy {
                element_type: T::TY,
                shape: SmallVec::from_slice(D::dims().as_ref()),
            },
            format!("param_{}", i),
        );

        params.push(inner.clone());
        let tensor_index = builder.mut_params.push(
            Tensor {
                inner,
                phantom: PhantomData,
            }
            .into(),
        );
        // Safety: Boxcar ensures that the pointers are fixed, since it never reallocates.
        // We also do not take a new reference of this type, until the `CompFn` has been called
        let tensor = unsafe { &mut *builder.mut_params[tensor_index].get() };
        // Safety: since we created the inner op above with the correct type and dimension, we can
        // guarentee that this is correct
        unsafe { tensor.unsafe_mut_cast() }
    }

    fn is_mut_borrowed() -> bool {
        true
    }
}

// This macro allows us to implement `CompFn` for a series of tuples easily.
// This essentially a workaround for Rust lacking variadic types / generics.
macro_rules! impl_comp_fn {
      ($($ty:tt),*) => {
          paste! {
            #[allow(non_snake_case, unused_variables, unused_mut)]
            impl<F, $($ty,)* R> CompFn<($($ty, )*), R> for F
            where
                F: Sync + Send,
                F: Fn($($ty, )*) -> R,
                F: for<'a> Fn($(<$ty as FromBuilder>::Item<'a>, )*) -> R ,
                $($ty: FromBuilder, )*
            {

                fn compute(&self, builder: &mut Builder) -> R {
                    let mut alias_index = $({
                      let $ty = 1;
                      $ty
                    } + )* 1;
                    $(
                      let param_index = builder.params.borrow().len();
                      if $ty::is_mut_borrowed() {
                        alias_index -= 1;
                        // TODO(sphw): add alias back
                        builder.setup_alias(param_index as u64, alias_index);
                      }
                    )*
                    $(let $ty = $ty::from_builder(builder);)*
                    let res = (self)($($ty,)*);
                    res
                }
            }
        }
      };
  }

impl_comp_fn!();
impl_comp_fn!(T1);
impl_comp_fn!(T1, T2);
impl_comp_fn!(T1, T2, T3);
impl_comp_fn!(T1, T2, T3, T4);
impl_comp_fn!(T1, T2, T3, T4, T5);
impl_comp_fn!(T1, T2, T3, T4, T5, T6);
impl_comp_fn!(T1, T2, T3, T4, T5, T6, T7);
impl_comp_fn!(T1, T2, T3, T4, T5, T6, T7, T8);
impl_comp_fn!(T1, T2, T3, T4, T5, T6, T7, T9, T10);
impl_comp_fn!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11);
impl_comp_fn!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12);
impl_comp_fn!(T1, T2, T3, T4, T5, T6, T7, T9, T10, T11, T12, T13);
