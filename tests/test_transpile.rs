use insta::assert_snapshot;
use prql::transpile;
use prql::Result;

#[test]
fn transpile_variables() -> Result<()> {
    assert_snapshot!(transpile("select 1")?, @r###"
    SELECT
      1
    "###);

    assert_snapshot!(transpile(
r#"
from employees
filter country = "USA"                           # Each line transforms the previous result.
derive [                                         # This adds columns / variables.
  gross_salary: salary + payroll_tax,
  gross_cost:   gross_salary + benefits_cost     # Variables can use other variables.
]
filter gross_cost > 0
aggregate by:[title, country] [                  # `by` are the columns to group by.
    average salary,                              # These are aggregation calcs run on each group.
    sum     salary,
    average gross_salary,
    sum     gross_salary,
    average gross_cost,
    sum_gross_cost: sum gross_cost,
    count: count,
]
sort sum_gross_cost
filter count > 200
take 20
"#)?, @r###"
    SELECT
      TOP (20) SUM(salary + payroll_tax + benefits_cost) AS sum_gross_cost,
      COUNT(*) AS count,
      AVG(salary),
      SUM(salary),
      AVG(salary + payroll_tax),
      SUM(salary + payroll_tax),
      AVG(salary + payroll_tax + benefits_cost)
    FROM
      employees
    WHERE
      country = 'USA'
      and salary + payroll_tax + benefits_cost > 0
    GROUP BY
      title,
      country
    HAVING
      COUNT(*) > 200
    ORDER BY
      SUM(salary + payroll_tax + benefits_cost)
    "###);

    Ok(())
}

#[test]
fn transpile_functions() -> Result<()> {
    // TODO: Compare to canoncial example:
    // - Window func not yet built.
    let prql = r#"
    func lag_day x = s"lag_day_todo({x})"
    func ret x = x / (lag_day x) - 1 + dividend_return
    func if_valid x = s"IF(is_valid_price, {x}, NULL)"

    from prices
    derive [
      return_total:      if_valid (ret prices_adj),
    ]
    "#;
    let result = transpile(prql)?;

    assert_snapshot!(result, @r###"
    SELECT
      IF(
        is_valid_price,
        prices_adj / lag_day_todo(prices_adj) - 1 + dividend_return,
        NULL
      ) AS return_total,
      *
    FROM
      prices
    "###);

    // Assert that the nested function has been run.
    assert!(!result.contains(&"ret prices_adj"));
    assert!(!result.contains(&"lag_day prices_adj"));

    Ok(())
}

#[test]
fn transpile_joins() -> Result<()> {
    // TODO: issues, as outlined in https://github.com/max-sixty/prql/issues/194
    // - from the point where alias emp_salary is introduced for average salary,
    //   we should use emp_salary instead of the function call. This currently
    //   causes problems when declaring average emp_salary producing
    //   AVG(AVG(salary)). This means that we have to introduce "scopes" that
    //   encapsulates each of the atomic pipelines.
    // - we need table aliases in joins and froms because joins get real long.
    //   Also, I think that using only emp_no in second select will not resolve
    //   to table_0.emp_no.
    //   - Possibly use `USING` to make these even shorter,
    //     though there are very small differences between inner & equi joins.
    //     Though probably we should favor equi-joins.

    assert_snapshot!(transpile(r#"
    from employees
join side:left salaries [salaries.emp_no = employees.emp_no]
aggregate by:[employees.emp_no] [
  emp_salary: average salary
]
join side:left titles [titles.emp_no = emp_no]
join dept_emp [dept_emp.emp_no = emp_no]
aggregate by:[dept_emp.dept_no, titles.title] [
  avg_salary: average emp_salary
]
join side:left departments [departments.dept_no = dept_no]
select [dept_name, title, avg_salary]
"#)?, @r###"
    WITH table_0 AS (
      SELECT
        AVG(salary) AS emp_salary
      FROM
        employees
        LEFT JOIN salaries ON salaries.emp_no = employees.emp_no
      GROUP BY
        employees.emp_no
    ),
    table_1 AS (
      SELECT
        AVG(AVG(salary)) AS avg_salary
      FROM
        table_0
        LEFT JOIN titles ON titles.emp_no = emp_no
        JOIN dept_emp ON dept_emp.emp_no = emp_no
      GROUP BY
        dept_emp.dept_no,
        titles.title
    ),
    table_2 AS (
      SELECT
        dept_name,
        title,
        AVG(AVG(salary))
      FROM
        table_1
        LEFT JOIN departments ON departments.dept_no = dept_no
    )
    SELECT
      *
    FROM
      table_2
    "###);

    Ok(())
}
